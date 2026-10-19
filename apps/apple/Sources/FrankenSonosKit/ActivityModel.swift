import Foundation

/// Read-only history session. It never acknowledges, undoes or retries a
/// speaker command. Returned history selections are one-observation tokens.
@MainActor public final class ActivityModel {
    public struct Entry: Identifiable, Sendable {
        public let id = UUID()
        public let play: RecordedPlay
    }
    public enum Replay: String, CaseIterable, Sendable { case play, queueNext, queueLast }
    public var onChange: (@MainActor () -> Void)?
    public private(set) var active = false { willSet { onChange?() } }
    public private(set) var loading = false { willSet { onChange?() } }
    public private(set) var query = ActivityQuery() { willSet { onChange?() } }
    public private(set) var actions: [LoggedAction] = [] { willSet { onChange?() } }
    public private(set) var history: [Entry] = [] { willSet { onChange?() } }
    public private(set) var actionsObservedAt: Date? { willSet { onChange?() } }
    public private(set) var historyObservedAt: Date? { willSet { onChange?() } }
    public private(set) var actionsProblem: String? { willSet { onChange?() } }
    public private(set) var historyProblem: String? { willSet { onChange?() } }
    public private(set) var filterProblem: String? { willSet { onChange?() } }
    public private(set) var actionsStale = true { willSet { onChange?() } }
    public private(set) var historyStale = true { willSet { onChange?() } }
    private let client: DaemonClient
    private var task: Task<Void, Never>?
    private var generation: UInt64 = 0
    private var pending = false
    private var stopped = false

    public init(client: DaemonClient) { self.client = client }
    public func resume() {
        guard !active && !stopped else { return }
        active = true; refresh()
    }
    public func suspend() {
        active = false; generation &+= 1; pending = false
        actionsStale = true; historyStale = true; task?.cancel()
    }
    public func stop() { suspend(); stopped = true }
    public func apply(_ requested: ActivityQuery) {
        do { _ = try requested.validated() }
        catch { filterProblem = error.localizedDescription; return }
        guard !stopped else { return }
        filterProblem = nil
        guard query != requested else { refresh(); return }
        generation &+= 1; task?.cancel(); query = requested
        actions = []; history = []; actionsObservedAt = nil; historyObservedAt = nil
        actionsProblem = nil; historyProblem = nil
        actionsStale = true; historyStale = true
        if active { pending = true; start() }
    }
    public func refresh() {
        guard active && !stopped else { return }
        pending = true; actionsStale = true; historyStale = true; start()
    }

    /// Offers a supported source, never a target inferred from a history row.
    /// The caller must select a current group and use CommandReviewSession.
    public func replayCommand(_ id: UUID, as mode: Replay) throws -> NativeCommand {
        guard active && !loading && !pending && !historyStale && historyProblem == nil,
              let entry = history.first(where: { $0.id == id }), let source = entry.play.replaySource else {
            throw ContractError.invalid("Refresh history and choose a current, portable source before reviewing playback.")
        }
        switch mode {
        case .play: return .playSource(source)
        case .queueNext: return .queueAdd(source, next: true)
        case .queueLast: return .queueAdd(source, next: false)
        }
    }

    private func start() {
        guard active && !stopped && pending && task == nil else { return }
        pending = false; loading = true
        let token = generation, client = client, query = query
        task = Task { [weak self] in
            await withTaskGroup(of: ActivityReply.self) { group in
                group.addTask {
                    do { try Task.checkCancellation(); return .actions(try await client.recentActions(query), nil) }
                    catch { return .actions(nil, error.localizedDescription) }
                }
                group.addTask {
                    do { try Task.checkCancellation(); return .history(try await client.recentPlays(query), nil) }
                    catch { return .history(nil, error.localizedDescription) }
                }
                for await reply in group {
                    guard let self, self.active && !self.stopped && self.generation == token && !Task.isCancelled else { continue }
                    switch reply {
                    case .actions(let rows, let problem):
                        self.actionsProblem = problem; self.actionsStale = problem != nil
                        if let rows { self.actions = rows; self.actionsObservedAt = Date() }
                    case .history(let rows, let problem):
                        self.historyProblem = problem; self.historyStale = problem != nil
                        if let rows { self.history = rows.map { Entry(play: $0) }; self.historyObservedAt = Date() }
                    }
                }
            }
            // Even an abort-ignoring transport retains its slot until both
            // reads drain. Rapid reopen/filter changes never stack old batches.
            self?.task = nil; self?.loading = false
            self?.start()
        }
    }
    deinit { task?.cancel() }
}

private enum ActivityReply: Sendable {
    case actions([LoggedAction]?, String?)
    case history([RecordedPlay]?, String?)
}
