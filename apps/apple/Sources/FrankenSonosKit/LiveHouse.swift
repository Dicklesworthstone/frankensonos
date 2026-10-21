import Foundation

/// Every update is a complete view. A slow UI may skip intermediate display
/// states, but cannot lose a delta required to reconstruct the latest one.
public struct LiveHouseView: Sendable {
    public enum Phase: String, Sendable { case suspended, connecting, refreshing, live, retrying }
    public let snapshot: HouseSnapshot?
    public let phase: Phase
    public let problem: String?
    /// Connection health is distinct from an individual speaker's availability.
    public let isConnected: Bool
    /// Settled observations, excluding known invalidations and in-flight reads.
    public let freshRooms: Set<RoomID>

    init(snapshot: HouseSnapshot?, phase: Phase, problem: String?,
         isConnected: Bool? = nil, freshRooms: Set<RoomID>? = nil) {
        self.snapshot = snapshot; self.phase = phase; self.problem = problem
        self.isConnected = phase != .suspended && (isConnected ?? (phase == .live))
        if self.isConnected, let snapshot {
            let observed = Set(snapshot.states.keys).intersection(snapshot.stateObservedAt.keys)
                .intersection(snapshot.zones.flatMap(\.rooms)).subtracting(snapshot.failures.keys)
            self.freshRooms = (freshRooms ?? (phase == .live ? observed : [])).intersection(observed)
        } else { self.freshRooms = [] }
    }

    /// Display freshness only, never authority to skip a command's fresh reads.
    public func isFresh(for room: RoomID) -> Bool { freshRooms.contains(room) }
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
    private var readingAll = false, readingRooms = Set<RoomID>()
    private var retryPending = false
    private var retryID: UInt64 = 0
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
        cancelReadRetry()
        // Keep the read slot until its cancelled operation has drained. A
        // rapid resume must not overlap it with another four-member batch.
        readTask?.cancel()
        full = false; names.removeAll()
        publish(.suspended)
    }

    public func refresh() {
        guard active else { return }
        cancelReadRetry()
        invalidate()
    }

    public func stop() {
        guard !stopped else { return }
        suspend(); stopped = true; output.finish()
    }

    private func publish(_ phase: LiveHouseView.Phase) {
        var fresh = Set<RoomID>()
        if active && connected && !full && !readingAll, let snapshot {
            fresh = Set(snapshot.states.keys).subtracting(readingRooms)
                .subtracting(snapshot.affected(by: names))
        }
        output.yield(LiveHouseView(snapshot: snapshot, phase: phase, problem: problem,
                                  isConnected: active && connected, freshRooms: fresh))
    }

    private func invalidate(_ room: String? = nil) {
        guard active else { return }
        if let room, !room.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
           room.utf8.count <= 512, names.count < 64, !full, let snapshot,
           snapshot.zones.contains(where: { $0.members.contains(room) }) {
            // A notification is not evidence that an unavailable room can be
            // read now. Its retry stays scheduled; healthy group members and
            // namesakes in other households still receive the notification.
            if !snapshot.affected(by: [room]).isDisjoint(with: snapshot.states.keys) {
                names.insert(room)
            }
        } else { full = true; names.removeAll() }
        publish(problem == nil ? (connected ? .refreshing : .connecting) : .retrying)
        scheduleRead()
    }

    private func scheduleRead() {
        guard active, readTask == nil, flushTask == nil,
              full || retryPending || !names.isEmpty else { return }
        // An unknown topology blocks targeted reads. A room outage alone does
        // not: independent healthy updates must not wait for its retry timer.
        guard !full || readRetry == nil else { return }
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
        guard readTask == nil, !full || readRetry == nil else { return }
        let all = full, retry = retryPending, requested = names, previous = snapshot
        full = false; retryPending = false; names.removeAll(); readID &+= 1
        // Retain the consumed invalidation until its batch finishes. A later
        // event must not accidentally label this still-running read as fresh.
        readingAll = all || previous == nil
        readingRooms = previous?.affected(by: requested) ?? []
        let id = readID, client = client
        readTask = Task { [weak self] in
            do {
                let value: HouseSnapshot
                if !all, let previous {
                    let rooms = previous.affected(by: requested)
                    if retry {
                        value = try await client.recoverHouse(previous, refreshing: rooms)
                    } else {
                        // The single read slot also covers recovery. Never
                        // poll failed members again as a healthy sibling plays.
                        value = try await client.refresh(previous, rooms: rooms.subtracting(previous.failures.keys))
                    }
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
        readTask = nil; readingAll = false; readingRooms.removeAll()
        guard active && generation == token else { scheduleRead(); return }
        if let value { snapshot = value }
        if let error { problem = error }
        else if value?.isComplete == false { problem = "Some rooms could not be refreshed; retrying." }
        else if connected { problem = nil }
        if error != nil {
            full = true; names.removeAll()
            armReadRetry()
        } else if value?.isComplete == false {
            // A healthy-room refresh retains unrelated failures; it neither
            // resets their deadline nor consumes another backoff interval.
            armReadRetry()
        } else {
            readDelay = timing.firstRetry
            cancelReadRetry()
        }
        if connected && problem == nil && !full && !retryPending && names.isEmpty {
            feedDelay = timing.firstRetry
            publish(.live)
        } else { publish(problem == nil ? .refreshing : .retrying) }
        scheduleRead()
    }

    private func cancelReadRetry() {
        retryID &+= 1
        readRetry?.cancel(); readRetry = nil; retryPending = false
    }

    private func armReadRetry() {
        guard readRetry == nil && !retryPending else { return }
        retryID &+= 1
        let delay = readDelay, token = generation, retry = retryID
        readDelay = min(timing.maxRetry, readDelay * 2)
        readRetry = Task { [weak self] in
            do {
                try await Task.sleep(for: delay)
                await self?.retryRead(generation: token, retry: retry)
            } catch { }
        }
    }

    private func retryRead(generation token: UInt64, retry: UInt64) {
        guard active && generation == token && retryID == retry else { return }
        readRetry = nil; retryPending = true
        scheduleRead()
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
