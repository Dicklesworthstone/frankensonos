import Foundation

/// Search is explicit and single-flight. Edits and suspension retire result
/// authority, while a cancelled transport keeps its slot until it drains.
@MainActor public final class SpotifySearchModel {
    public typealias Reader = @Sendable (SpotifySearchRequest) async throws -> SpotifySearchPage
    public var onChange: (@MainActor () -> Void)?
    public private(set) var query = "" { willSet { onChange?() } }
    public private(set) var kind: SpotifyCatalogKind = .track { willSet { onChange?() } }
    public private(set) var active = false { willSet { onChange?() } }
    public private(set) var loading = false { willSet { onChange?() } }
    public private(set) var isCurrent = false { willSet { onChange?() } }
    public private(set) var observation: SpotifySearchObservation? { willSet { onChange?() } }
    public private(set) var problem: String? { willSet { onChange?() } }
    private let read: Reader
    private let now: @Sendable () -> Date
    private var task: Task<Void, Never>?
    private var epoch: UInt64 = 0
    private var stopped = false

    public init(read: @escaping Reader, now: @escaping @Sendable () -> Date = { Date() }) {
        self.read = read; self.now = now
    }
    public convenience init(client: DaemonClient) { self.init { try await client.searchSpotify($0) } }

    public var canSearch: Bool {
        active && !stopped && !loading && (try? SpotifySearchRequest(query: query, kind: kind)) != nil
    }
    public var canChoose: Bool { active && !stopped && !loading && isCurrent && observation != nil }

    public func resume() { if !stopped { active = true } }
    public func edit(_ value: String) {
        guard !stopped, !query.utf8.elementsEqual(value.utf8) else { return }
        invalidate(); query = value; problem = nil
    }
    public func chooseKind(_ value: SpotifyCatalogKind) {
        guard !stopped, kind != value else { return }
        invalidate(); kind = value; problem = nil
    }
    public func cancel() {
        guard !stopped else { return }
        invalidate(); problem = "Search cancelled. Search again when you’re ready."
    }
    public func suspend() { active = false; invalidate() }
    public func stop() { suspend(); stopped = true; observation = nil; problem = nil }

    public func search() {
        guard canSearch, task == nil else { return }
        do {
            let request = try SpotifySearchRequest(query: query, kind: kind)
            invalidate(); loading = true; problem = nil
            let token = epoch, read = read, now = now
            task = Task { [weak self] in
                defer { self?.task = nil; self?.loading = false }
                do {
                    try Task.checkCancellation()
                    let page = try await read(request).validated(for: request)
                    try Task.checkCancellation()
                    guard let self, self.active && !self.stopped && self.epoch == token else { return }
                    self.observation = SpotifySearchObservation(request: request, page: page, receivedAt: now())
                    self.isCurrent = true
                } catch {
                    guard let self, self.active && !self.stopped && self.epoch == token && !Task.isCancelled else { return }
                    self.problem = error.localizedDescription
                }
            }
        } catch { problem = error.localizedDescription }
    }

    public func selection(_ item: SpotifySearchItem, from id: UUID, in zone: Zone,
                          action: SpotifySearchSelection.Action) throws -> SpotifySearchSelection {
        guard canChoose, let observation, observation.id == id,
              observation.page.items.contains(item) else {
            throw ContractError.invalid("This Spotify result is no longer current. Search again before choosing music.")
        }
        _ = try zone.validated()
        return SpotifySearchSelection(item: item, zone: zone, action: action)
    }

    private func invalidate() {
        epoch &+= 1; isCurrent = false; task?.cancel()
    }
    deinit { task?.cancel() }
}
