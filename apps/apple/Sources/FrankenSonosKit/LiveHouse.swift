import Foundation

/// Every update is a complete view. A slow UI may skip intermediate display
/// states, but cannot lose a delta required to reconstruct the latest one.
public struct LiveHouseView: Sendable {
    public enum Phase: String, Sendable { case suspended, connecting, refreshing, live, retrying }
    public let snapshot: HouseSnapshot?
    public let phase: Phase
    public let problem: String?
    public var isFresh: Bool { phase == .live && snapshot?.isComplete == true }
}

/// One native client's read-only live session. Resume on foreground entry;
/// suspend on background entry; stop when the view/session is discarded.
/// This actor never performs, retries or acknowledges a playback command.
public actor LiveHouse {
    public nonisolated let updates: AsyncStream<LiveHouseView>
    private let output: AsyncStream<LiveHouseView>.Continuation
    private let client: DaemonClient
    private let timing: Timing
    private var active = false, stopped = false, connected = false
    private var generation: UInt64 = 0, readID: UInt64 = 0
    private var snapshot: HouseSnapshot?
    private var full = false, names = Set<String>()
    private var cursor: String?
    private var problem: String?
    private var feedDelay: Duration, readDelay: Duration
    private var feedTask: Task<Void, Never>?, readTask: Task<Void, Never>?
    private var flushTask: Task<Void, Never>?, feedRetry: Task<Void, Never>?, readRetry: Task<Void, Never>?

    struct Timing: Sendable {
        var coalesce: Duration = .milliseconds(250)
        var firstRetry: Duration = .seconds(1)
        var maxRetry: Duration = .seconds(60)
    }

    public init(client: DaemonClient) { self.init(client: client, timing: Timing()) }
    init(client: DaemonClient, timing: Timing) {
        self.client = client; self.timing = timing
        feedDelay = timing.firstRetry; readDelay = timing.firstRetry
        let pair = AsyncStream<LiveHouseView>.makeStream(bufferingPolicy: .bufferingNewest(1))
        updates = pair.stream; output = pair.continuation
        output.yield(LiveHouseView(snapshot: nil, phase: .suspended, problem: nil))
        output.onTermination = { @Sendable [weak self] _ in Task { await self?.stop() } }
    }

    public func resume() {
        guard !active && !stopped else { return }
        active = true; generation &+= 1
        connected = false; problem = nil
        feedDelay = timing.firstRetry; readDelay = timing.firstRetry
        openFeed(); invalidate()
    }

    public func suspend() {
        guard !stopped else { return }
        active = false; connected = false; generation &+= 1
        feedTask?.cancel(); feedTask = nil
        flushTask?.cancel(); flushTask = nil
        feedRetry?.cancel(); feedRetry = nil
        readRetry?.cancel(); readRetry = nil
        // Keep the read slot until its cancelled operation has drained. A
        // rapid resume must not overlap it with another four-member batch.
        readTask?.cancel()
        full = false; names.removeAll()
        publish(.suspended)
    }

    public func refresh() {
        guard active else { return }
        readRetry?.cancel(); readRetry = nil
        invalidate()
    }

    public func stop() {
        guard !stopped else { return }
        suspend(); stopped = true; output.finish()
    }

    private func publish(_ phase: LiveHouseView.Phase) {
        output.yield(LiveHouseView(snapshot: snapshot, phase: phase, problem: problem))
    }

    private func invalidate(_ room: String? = nil) {
        guard active else { return }
        if let room, !room.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
           room.utf8.count <= 512, names.count < 64, !full {
            names.insert(room)
        } else { full = true; names.removeAll() }
        publish(problem == nil ? (connected ? .refreshing : .connecting) : .retrying)
        scheduleRead()
    }

    private func scheduleRead() {
        guard active, readTask == nil, flushTask == nil, readRetry == nil,
              full || !names.isEmpty else { return }
        let token = generation, delay = timing.coalesce
        // A fixed first-event deadline, not a debounce that a continuous
        // stream can keep postponing. At most one read batch runs at a time.
        flushTask = Task { [weak self] in
            do {
                try await Task.sleep(for: delay)
                await self?.beginRead(generation: token)
            } catch { }
        }
    }

    private func beginRead(generation token: UInt64) {
        guard active && generation == token else { return }
        flushTask = nil
        guard readTask == nil else { return }
        let all = full, requested = names, previous = snapshot
        full = false; names.removeAll(); readID &+= 1
        let id = readID, client = client
        readTask = Task { [weak self] in
            do {
                let value: HouseSnapshot
                if !all, let previous {
                    let rooms = previous.affected(by: requested)
                    // Unknown names may be a renamed room or a new group.
                    let known = Set(previous.zones.flatMap(\.members))
                    if !rooms.isEmpty && requested.isSubset(of: known) {
                        value = try await client.refresh(previous, rooms: rooms)
                    } else { value = try await client.houseSnapshot() }
                } else { value = try await client.houseSnapshot() }
                try Task.checkCancellation()
                await self?.readFinished(value, error: nil, id: id, generation: token)
            } catch {
                await self?.readFinished(nil, error: error.localizedDescription, id: id, generation: token)
            }
        }
    }

    private func readFinished(_ value: HouseSnapshot?, error: String?, id: UInt64, generation token: UInt64) {
        guard readID == id else { return }
        readTask = nil
        guard active && generation == token else { scheduleRead(); return }
        if let value { snapshot = value }
        if let error { problem = error }
        else if value?.isComplete == false { problem = "Some rooms could not be refreshed; retrying." }
        else if connected { problem = nil }
        if error != nil || value?.isComplete == false {
            full = true; names.removeAll()
            armReadRetry()
        } else { readDelay = timing.firstRetry }
        if connected && problem == nil && !full && names.isEmpty {
            feedDelay = timing.firstRetry
            publish(.live)
        } else { publish(problem == nil ? .refreshing : .retrying) }
        scheduleRead()
    }

    private func armReadRetry() {
        guard readRetry == nil else { return }
        let delay = readDelay, token = generation
        readDelay = min(timing.maxRetry, readDelay * 2)
        readRetry = Task { [weak self] in
            do {
                try await Task.sleep(for: delay)
                await self?.retryRead(generation: token)
            } catch { }
        }
    }

    private func retryRead(generation token: UInt64) {
        guard active && generation == token else { return }
        readRetry = nil; invalidate()
    }

    private func openFeed() {
        guard active && feedTask == nil else { return }
        let token = generation, client = client, after = cursor
        feedTask = Task { [weak self] in
            do {
                let stream = try client.eventStream(after: after)
                for try await signal in stream {
                    try Task.checkCancellation()
                    await self?.receive(signal, generation: token)
                }
                try Task.checkCancellation()
                await self?.feedEnded("Live connection ended; reconnecting.", reset: false, generation: token)
            } catch {
                if !Task.isCancelled {
                    await self?.feedEnded(error.localizedDescription,
                                          reset: (error as? ClientError) == .eventOverflow, generation: token)
                }
            }
        }
    }

    private func receive(_ signal: EventSignal, generation token: UInt64) {
        guard active && generation == token else { return }
        switch signal {
        case .connected:
            connected = true; problem = nil
            // Every connection gets a new full snapshot, even with a replay
            // cursor. Events received during that read stay pending afterwards.
            invalidate()
        case .event(let event):
            if let id = event.id, UInt64(id) != nil { cursor = id }
            switch event.kind {
            case "events.reset": cursor = nil; invalidate()
            case "topology.changed", "player.health": invalidate()
            case "zone.state", "dj.status":
                let field = event.kind == "zone.state" ? "room" : "zone"
                let body = try? JSONDecoder().decode([String: JSONValue].self, from: Data(event.data.utf8))
                if case .string(let room) = body?[field] { invalidate(room) }
                else { invalidate() }
            default: break
            }
        }
    }

    private func feedEnded(_ error: String, reset: Bool, generation token: UInt64) {
        guard active && generation == token else { return }
        feedTask = nil; connected = false; problem = error
        if reset { cursor = nil }
        invalidate()
        let delay = feedDelay
        feedDelay = min(timing.maxRetry, feedDelay * 2)
        feedRetry = Task { [weak self] in
            do {
                try await Task.sleep(for: delay)
                await self?.reconnect(generation: token)
            } catch { }
        }
    }

    private func reconnect(generation token: UInt64) {
        guard active && generation == token else { return }
        feedRetry = nil; openFeed()
        // Read-only fallback remains useful even while SSE cannot connect.
        invalidate()
    }

    deinit {
        feedTask?.cancel(); readTask?.cancel(); flushTask?.cancel()
        feedRetry?.cancel(); readRetry?.cancel(); output.finish()
    }
}
