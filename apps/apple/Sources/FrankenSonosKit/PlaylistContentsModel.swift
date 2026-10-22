import Foundation

public struct PlaylistContentsObservation: Sendable, Identifiable {
    public let id: UUID
    public let request: PlaylistContentsRequest
    public let page: PlaylistContentsPage
    public let receivedAt: Date
}

/// One bounded page and one reader. Opening, filtering and paging never write.
/// Cancelled reads retain the slot until they drain; retained pages lose all selection authority.
@MainActor public final class PlaylistContentsModel {
    public typealias Reader = @Sendable (PlaylistContentsRequest) async throws -> PlaylistContentsPage
    public var onChange: (@MainActor () -> Void)?
    public private(set) var isOpen = false { willSet { onChange?() } }
    public private(set) var active = false { willSet { onChange?() } }
    public private(set) var loading = false { willSet { onChange?() } }
    public private(set) var isCurrent = false { willSet { onChange?() } }
    public private(set) var query = "" { willSet { onChange?() } }
    public private(set) var playlistUri: String? { willSet { onChange?() } }
    public private(set) var name = "" { willSet { onChange?() } }
    public private(set) var observation: PlaylistContentsObservation? { willSet { onChange?() } }
    public private(set) var problem: String? { willSet { onChange?() } }
    private let read: Reader
    private var generation: UInt64 = 0
    private var stopped = false
    private var task: Task<Void, Never>?
    private var pending: PlaylistContentsRequest?

    public init(read: @escaping Reader) { self.read = read }
    public convenience init(client: DaemonClient) { self.init { try await client.playlistContents($0) } }
    public var canRefresh: Bool {
        guard active && isOpen && !stopped && !loading, let playlistUri else { return false }
        return (try? PlaylistContentsRequest(playlistUri: playlistUri, query: query)) != nil
    }
    public var canChoose: Bool { canRefresh && isCurrent && observation != nil }
    public var canGoForward: Bool { canChoose && observation?.page.nextOffset != nil }
    public var canGoBack: Bool { canChoose && (observation?.request.offset ?? 0) > 0 }

    /// Caller resolves its captured catalog selection before opening this detail.
    public func open(playlistUri: String, name: String) throws {
        guard !stopped else { throw ContractError.invalid("This playlist browser has stopped.") }
        let request = try PlaylistContentsRequest(playlistUri: playlistUri)
        invalidate()
        self.playlistUri = playlistUri; self.name = name; query = ""
        observation = nil; problem = nil; isOpen = true; active = true
        enqueue(request)
    }
    public func edit(_ text: String) {
        guard active && isOpen && !stopped, text != query else { return }
        query = text; invalidate(); problem = nil
    }
    public func refresh() {
        guard canRefresh, let playlistUri else { return }
        do { enqueue(try PlaylistContentsRequest(playlistUri: playlistUri, query: query)) }
        catch { problem = error.localizedDescription }
    }
    public func nextPage() {
        guard canGoForward, let observed = observation, let offset = observed.page.nextOffset else { return }
        page(offset, from: observed)
    }
    public func previousPage() {
        guard canGoBack, let observed = observation else { return }
        page(observed.request.offset >= PlaylistContentsRequest.pageSize
             ? observed.request.offset - PlaylistContentsRequest.pageSize : 0, from: observed)
    }
    private func page(_ offset: UInt32, from observed: PlaylistContentsObservation) {
        do {
            enqueue(try PlaylistContentsRequest(playlistUri: observed.request.playlistUri, query: observed.request.query,
                                                offset: offset, expectedRevision: observed.page.revision))
        } catch { invalidate(); problem = error.localizedDescription }
    }
    public func selection(_ item: PlaylistTrack, from id: UUID, in zone: Zone,
                          action: PlaylistTrackSelection.Action) throws -> PlaylistTrackSelection {
        guard canChoose, let observed = observation, observed.id == id else {
            throw ContractError.invalid("This playlist page is no longer current. Read it again before choosing music.")
        }
        return try PlaylistTrackSelection(item: item, page: observed.page, request: observed.request, in: zone, action: action)
    }
    public func invalidate() {
        generation &+= 1; pending = nil; task?.cancel(); isCurrent = false
        loading = task != nil
    }
    public func cancel() { invalidate() }
    /// Returning to the foreground never silently rereads or revives a selection.
    public func resume() { if !stopped && isOpen { active = true } }
    public func suspend() { active = false; invalidate() }
    public func close() { suspend(); isOpen = false }
    public func stop() { close(); stopped = true; observation = nil; playlistUri = nil; name = ""; query = "" }

    private func enqueue(_ request: PlaylistContentsRequest) {
        invalidate(); problem = nil; pending = request; loading = true; pump()
    }
    private func pump() {
        guard active && isOpen && !stopped && task == nil, let request = pending else { return }
        pending = nil
        let token = generation, reader = read
        task = Task { [weak self] in
            do {
                try Task.checkCancellation()
                let page = try await reader(request).validated(for: request)
                try Task.checkCancellation()
                if let self, self.generation == token && self.active && self.isOpen && !self.stopped {
                    self.observation = PlaylistContentsObservation(id: UUID(), request: request, page: page, receivedAt: Date())
                    self.isCurrent = true; self.problem = nil
                }
            } catch {
                if let self, self.generation == token && self.active && self.isOpen && !self.stopped {
                    self.isCurrent = false; self.problem = error.localizedDescription
                }
            }
            self?.finish()
        }
    }
    private func finish() {
        task = nil; loading = pending != nil
        pump()
    }
}
