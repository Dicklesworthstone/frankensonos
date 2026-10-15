import Foundation
import Combine

@MainActor
final class LiveZoneStore: ZoneStore {
    @Published var zones: [AudioZone] = []
    @Published var selectedZoneID = UUID() {
        didSet { tick(now: Date()) }
    }
    @Published var rooms: [String] = []
    @Published var tracks: [Track] = []
    @Published var roomVolumes: [String: Double] = [:]
    @Published var connectionStatus: ZoneConnectionStatus = .offline {
        didSet { if oldValue != connectionStatus { AppLog.shared.add("status", "\(oldValue.rawValue) -> \(connectionStatus.rawValue)") } }
    }
    /// Set while the daemon answers over HTTP but refuses the live stream, so the app is current only as of the last
    /// refresh. Cleared when the stream opens.
    @Published var streamNote: String?
    @Published var commandError: String?
    @Published var commandSuggestions: [String] = []
    @Published var offlineRooms: Set<String> = []
    @Published var queueLengths: [String: Int] = [:]
    @Published var elapsed: Double = 0
    var lastEventID: String?
    let client: DaemonClient
    var roomRows: [RoomDTO] = []
    var zoneRows: [ZoneDTO] = []
    var states: [String: ZoneStateDTO] = [:]
    var positions: [UUID: (seconds: Double, at: Date)] = [:]
    var volumeEdits: [String: VolumeEdit] = [:]
    var draggingRooms: Set<String> = []
    var volumeTasks: [String: Task<Void, Never>] = [:]
    var commandTasks: [UUID: Task<Void, Never>] = [:]
    /// The latest command POST per room; the next one for that room waits for it.
    var commandChain: [String: Task<Void, Never>] = [:]
    var refetchTasks: [String: Task<Void, Never>] = [:]
    var topologyTask: Task<Void, Never>?
    var grouping = false
    /// The newest group request made while another was still in flight; it runs when that one finishes.
    var queuedGrouping: (rooms: [String], zoneID: UUID)?
    /// What the user last asked of a zone's transport, until the speaker confirms it or `holdSeconds` pass.
    var transportIntents: [UUID: (playing: Bool, until: Date)] = [:]
    static let holdSeconds: Double = 6
    var snapshotRevision = 0
    var lifecycleRevision = 0
    var streamOpen = false
    private var runner: Task<Void, Never>?
    private var active = false
    private var hasConnected = false

    init(baseURL: URL = DaemonSettings.url) { client = DaemonClient(baseURL: baseURL) }

    func start() {
        guard runner == nil else { return }
        active = true
        lifecycleRevision += 1
        runner = Task { [weak self] in await self?.connect() }
    }
    func retry() { setActive(false); start() }
    func setActive(_ value: Bool) {
        guard value != active else { return }
        if value { start(); return }
        active = false
        lifecycleRevision += 1
        streamOpen = false
        runner?.cancel()
        runner = nil
        topologyTask?.cancel()
        topologyTask = nil
        refetchTasks.values.forEach { $0.cancel() }
        refetchTasks = [:]
        commandTasks.values.forEach { $0.cancel() }
        commandTasks = [:]
        commandChain.values.forEach { $0.cancel() }
        commandChain = [:]
        volumeTasks.values.forEach { $0.cancel() }
        volumeTasks = [:]
        for (room, edit) in volumeEdits { roomVolumes[room] = edit.original }
        volumeEdits = [:]
        draggingRooms = []
        grouping = false
        queuedGrouping = nil
        rebuildZones()
        connectionStatus = hasConnected ? .reconnecting : .offline
    }

    private func connect() async {
        var attempt = 0
        while !Task.isCancelled && active {
            var streaming = false
            do {
                // A retry while the app is already showing live data should not flash "Refreshing".
                if hasConnected && connectionStatus != .live { connectionStatus = .refreshing }
                try await refreshAll()
                try Task.checkCancellation()
                // With nothing missed since the resume cursor, the daemon sends no headers until its next
                // heartbeat (up to 15 s). The data is fresh now, so do not wait for the stream to say so.
                hasConnected = true
                connectionStatus = .live
                streaming = true
                try await client.events(lastEventID: lastEventID, opened: {
                    self.hasConnected = true
                    self.streamOpen = true
                    self.streamNote = nil
                    self.connectionStatus = .live
                    attempt = 0
                }, receive: { event in await self.apply(event) })
            } catch {
                if Task.isCancelled || !active { return }
                streamOpen = false
                if streaming, let failure = error as? DaemonFailure, (400..<500).contains(failure.status) {
                    // The daemon is reachable and said no (a policy, usually). HTTP works, so the app is not
                    // disconnected: say why updates are not live and keep refreshing on the retry cadence.
                    streamNote = "Live updates are off: \(failure.detail)"
                    connectionStatus = .live
                } else {
                    streamNote = nil
                    AppLog.shared.add("stream", "events ended: \(error.localizedDescription)")
                    connectionStatus = hasConnected ? .reconnecting : .offline
                }
                // The ring is volatile. A confirmed daemon outage needs a fresh cursor.
                do { try await client.health() } catch { lastEventID = nil }
                let delay = DaemonClient.backoff[min(attempt, DaemonClient.backoff.count - 1)]
                attempt += 1
                do { try await Task.sleep(for: .seconds(delay)) } catch { return }
            }
        }
    }

    func tick(now: Date = Date()) {
        guard let zone = zones.first(where: { $0.id == selectedZoneID }), let base = positions[zone.id] else {
            elapsed = 0
            return
        }
        let current = base.seconds + (zone.isPlaying ? max(0, now.timeIntervalSince(base.at)) : 0)
        elapsed = zone.track.duration > 0 ? min(zone.track.duration, current) : current
    }

    func showFailure(_ error: Error) {
        let failure = error as? DaemonFailure
        commandSuggestions = failure?.suggestions ?? []
        if let failure, (400..<500).contains(failure.status) {
            commandError = failure.errorDescription
        } else {
            commandError = "Didn't go through. \(error.localizedDescription)"
        }
    }

    func roomKey(_ room: RoomDTO) -> String {
        roomRows.filter { $0.name == room.name }.count > 1 ? "\(room.name)@\(room.household)" : room.name
    }
    func key(name: String, household: String) -> String {
        roomRows.first(where: { $0.name == name && $0.household == household }).map(roomKey) ?? name
    }
    func target(_ room: String) -> String {
        guard let row = roomRows.first(where: { roomKey($0) == room }) else { return room }
        return "\(row.name)@\(row.household)"
    }
    func coordinator(_ zoneID: UUID) -> String? {
        zones.first(where: { $0.id == zoneID })?.roomNames.first
    }
}

struct VolumeEdit {
    let original: Double?
    var level: Double
    var revision: Int
    var editedAt: Date
    var acknowledged: Int? = nil
}
