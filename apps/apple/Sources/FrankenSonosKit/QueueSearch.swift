import Foundation

/// One explicit search section, not a subscription or a playback request.
public struct QueueSearchRequest: Sendable, Equatable {
    public let zone: Zone
    public let query: String
    public let offset: UInt32
    public let limit: UInt32
    public let expectedRevision: String?

    public init(in zone: Zone, query: String, offset: UInt32 = 0,
                limit: UInt32 = 50, expectedRevision: String? = nil) throws {
        _ = try zone.validated()
        let trimmed = query.trimmingCharacters(in: .whitespacesAndNewlines)
        guard query.utf8.count <= 1_024, !query.unicodeScalars.contains(where: CharacterSet.controlCharacters.contains),
              trimmed.unicodeScalars.contains(where: CharacterSet.alphanumerics.contains),
              trimmed.split(whereSeparator: \.isWhitespace).count <= 32,
              (1...200).contains(limit), offset < UInt32.max,
              offset == 0 || expectedRevision != nil else {
            throw ContractError.invalid("Enter up to 32 search words (1024 bytes). Continue only with the previous queue revision.")
        }
        if let expectedRevision, !Self.validRevision(expectedRevision) {
            throw ContractError.invalid("The queue revision is unreadable. Start a new search.")
        }
        self.zone = zone; self.query = trimmed; self.offset = offset
        self.limit = limit; self.expectedRevision = expectedRevision
    }

    static func validRevision(_ value: String) -> Bool {
        !value.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty && value.utf8.count <= 1_024 &&
        !value.unicodeScalars.contains(where: CharacterSet.controlCharacters.contains)
    }

    var queryItems: [URLQueryItem] {
        var fields = [URLQueryItem(name: "q", value: query),
                      URLQueryItem(name: "offset", value: String(offset)),
                      URLQueryItem(name: "limit", value: String(limit))]
        if let expectedRevision { fields.append(URLQueryItem(name: "expected_revision", value: expectedRevision)) }
        return fields
    }
}

/// Search matches are not a consecutive queue page. Keep each absolute position
/// and never pass this array to a block move or removal as if it were contiguous.
public struct QueueSearchPage: Decodable, Sendable, Equatable {
    public let zone: String
    public let query: String
    public let total: UInt32
    public let revision: String
    public let offset: UInt32
    public let examined: UInt32
    public let nextOffset: UInt32?
    public let complete: Bool
    public let items: [QueueItem]

    public func validated(for request: QueueSearchRequest) throws -> Self {
        let end = UInt64(offset) + UInt64(examined)
        guard zone.utf8.elementsEqual(request.zone.coordinatorRoom.utf8),
              query.utf8.elementsEqual(request.query.utf8), offset == request.offset,
              QueueSearchRequest.validRevision(revision),
              request.expectedRevision.map({ $0.utf8.elementsEqual(revision.utf8) }) != false,
              items.count <= request.limit, examined <= 9_900,
              end <= UInt64(max(total, offset)),
              (offset >= total ? examined == 0 : examined > 0),
              complete == (end >= UInt64(total)),
              complete ? nextOffset == nil : (nextOffset.map(UInt64.init) == end && end > UInt64(offset)) else {
            throw ContractError.invalid("The queue search coverage or identity is inconsistent. Start a new search.")
        }
        var previous = offset
        for item in items {
            guard item.position > previous, UInt64(item.position) <= end,
                  [Optional(item.title), item.creator, item.album, item.uri].allSatisfy({ ($0?.utf8.count ?? 0) <= 16_384 }) else {
                throw ContractError.invalid("The queue search contains invalid occurrence positions or oversized metadata.")
            }
            previous = item.position
        }
        return self
    }

    public var coverage: String {
        examined == 0 ? "No entries at offset \(offset); \(total) entries in this queue."
            : "Examined positions \(UInt64(offset) + 1)–\(UInt64(offset) + UInt64(examined)) of \(total)."
    }
}

public struct QueueSearchObservation: Identifiable, Sendable {
    public let id = UUID()
    public let request: QueueSearchRequest
    public let page: QueueSearchPage
    public let observedAt: Date
}

/// Issued only from a current result. The usual command review will reread the
/// queue revision and occurrence before any speaker write. No URI is enqueued.
public struct QueueSearchSelection: Sendable {
    public let zone: Zone
    public let item: QueueItem
    public let revision: String
    public var command: NativeCommand { .queuePlay(item, revision: revision) }
}

/// One bounded result batch and one in-flight search. Edits and notifications
/// retire selections, not the shared writer. Nothing here starts music.
@MainActor public final class QueueSearchModel {
    public typealias Reader = @Sendable (QueueSearchRequest) async throws -> QueueSearchPage
    public var onChange: (@MainActor () -> Void)?
    public private(set) var query = "" { willSet { onChange?() } }
    public private(set) var observation: QueueSearchObservation? { willSet { onChange?() } }
    public private(set) var loading = false { willSet { onChange?() } }
    public private(set) var problem: String? { willSet { onChange?() } }
    public private(set) var active = false { willSet { onChange?() } }
    public private(set) var isCurrent = false { willSet { onChange?() } }
    private var zone: Zone?
    private let read: Reader
    private var task: Task<Void, Never>?
    private var epoch: UInt64 = 0
    private var stopped = false

    public init(read: @escaping Reader) { self.read = read }
    public convenience init(client: DaemonClient) { self.init(read: { try await client.searchQueue($0) }) }

    public var canSearch: Bool {
        active && !stopped && !loading && zone.map { (try? QueueSearchRequest(in: $0, query: query)) != nil } == true
    }
    public var canContinue: Bool {
        canSearch && isCurrent && observation?.page.nextOffset != nil
    }

    /// A clock tick or member ordering change is harmless. A different group,
    /// closing the view, or suspension invalidates even an identical old result.
    public func activate(in value: Zone?) {
        guard !stopped else { return }
        let valid = value.flatMap { try? $0.validated() }
        let changed = zone.map { old in valid.map { !old.sameGroup(as: $0) } ?? true } ?? (valid != nil)
        if changed { invalidate(); observation = nil; problem = nil }
        zone = valid; active = valid != nil
    }

    public func edit(_ value: String) {
        guard !stopped, query != value else { return }
        invalidate(); query = value; problem = nil
    }

    /// Event/reset/catalog changes are hints that older results need a new
    /// explicit search. They never launch reads, retries or commands themselves.
    public func invalidate() {
        epoch &+= 1; isCurrent = false; task?.cancel()
        // Keep the slot until even a cancellation-ignoring transport drains.
    }
    public func suspend() { active = false; invalidate() }
    public func stop() { suspend(); stopped = true; observation = nil; zone = nil }
    public func cancel() { invalidate() }

    public func search() {
        guard canSearch, let zone else { return }
        do { start(try QueueSearchRequest(in: zone, query: query)) }
        catch { problem = error.localizedDescription }
    }

    public func continueSearch() {
        guard canContinue, let old = observation, let offset = old.page.nextOffset else { return }
        do {
            start(try QueueSearchRequest(in: old.request.zone, query: old.request.query,
                                         offset: offset, limit: old.request.limit, expectedRevision: old.page.revision),
                  expectedTotal: old.page.total)
        } catch { invalidate(); problem = error.localizedDescription }
    }

    private func start(_ request: QueueSearchRequest, expectedTotal: UInt32? = nil) {
        guard active && !stopped && task == nil else { return }
        invalidate(); loading = true; problem = nil
        let token = epoch, read = read
        task = Task { [weak self] in
            defer { self?.task = nil; self?.loading = false }
            do {
                try Task.checkCancellation()
                let page = try await read(request).validated(for: request)
                try Task.checkCancellation()
                guard expectedTotal == nil || expectedTotal == page.total else {
                    throw ContractError.invalid("The queue length changed during continuation. Start a new search.")
                }
                guard let self, self.active && !self.stopped && self.epoch == token,
                      self.zone?.sameGroup(as: request.zone) == true else { return }
                self.observation = QueueSearchObservation(request: request, page: page, observedAt: Date())
                self.isCurrent = true
            } catch {
                guard let self, self.active && !self.stopped && self.epoch == token && !Task.isCancelled else { return }
                self.problem = error.localizedDescription
            }
        }
    }

    /// Bind a result button to its observation, not just its displayed row.
    public func selection(_ item: QueueItem, from id: UUID) throws -> QueueSearchSelection {
        guard active && !stopped && !loading && isCurrent,
              let observed = observation, observed.id == id,
              zone?.sameGroup(as: observed.request.zone) == true,
              observed.page.items.contains(item), item.playable,
              let uri = item.uri, !uri.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              !uri.unicodeScalars.contains(where: CharacterSet.controlCharacters.contains) else {
            throw ContractError.invalid("This search result is stale or has no usable queue identity. Search again.")
        }
        return QueueSearchSelection(zone: observed.request.zone, item: item, revision: observed.page.revision)
    }

    deinit { task?.cancel() }
}
