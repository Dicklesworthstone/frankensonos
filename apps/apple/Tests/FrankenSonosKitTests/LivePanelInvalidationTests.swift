import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

private func panelAction(_ intent: String, decision: String = "allow") throws -> String {
    let data = try JSONSerialization.data(withJSONObject: ["intent": intent, "decision": decision])
    return String(decoding: data, as: UTF8.self)
}
private func panelZone(_ household: String = "TEST-A") -> Zone {
    Zone(household: household, coordinatorRoom: "Studio", members: ["Studio"], transportState: "paused")
}
private func panelState(_ zone: Zone) -> ZoneState {
    ZoneState(zone: zone, transportState: "paused", volume: 20, mute: false,
              groupVolume: 20, track: nil, shuffle: false, repeat: "off")
}

private actor PanelEventTransport: DaemonTransport {
    let zones: [Zone]
    var requests: [URLRequest] = []
    var streams: [AsyncThrowingStream<EventSignal, Error>.Continuation] = []
    var holdState = false, blocked = false, failTopology = false
    var waiter: CheckedContinuation<Void, Never>?
    init(_ zones: [Zone]) { self.zones = zones }
    func hold() { holdState = true }
    func fail() { failTopology = true }
    func release() { holdState = false; blocked = false; waiter?.resume(); waiter = nil }
    func send(_ request: URLRequest) async throws -> HTTPReply {
        requests.append(request)
        let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
        if request.url?.path == "/zones" {
            if failTopology { throw ClientError.unreadable }
            return HTTPReply(data: try encoder.encode(zones), status: 200)
        }
        guard let zone = zones.first(where: { request.url?.path == "/zones/\($0.id.address)/state" }) else {
            throw ContractError.invalid("Unexpected panel event test request")
        }
        if holdState {
            holdState = false; blocked = true
            // Deliberately finish late even if the session cancels the read.
            await withCheckedContinuation { waiter = $0 }
        }
        return HTTPReply(data: try encoder.encode(panelState(zone)), status: 200)
    }
    nonisolated func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> {
        let pair = AsyncThrowingStream<EventSignal, Error>.makeStream()
        Task { await register(pair.continuation) }
        return pair.stream
    }
    private func register(_ output: AsyncThrowingStream<EventSignal, Error>.Continuation) {
        streams.append(output); output.yield(.connected)
    }
    func emit(_ kind: String, data: String = "{}", count: Int = 1) {
        for _ in 0..<count { streams.last?.yield(.event(ServerEvent(id: nil, kind: kind, data: data))) }
    }
    func disconnect() { streams.last?.finish(throwing: ClientError.unreadable) }
}
private actor PanelViews {
    var values: [LiveHouseView] = []
    var latest: LiveHouseView? { values.last }
    func append(_ value: LiveHouseView) { values.append(value) }
}
private func panelEventually(_ condition: @escaping @Sendable () async -> Bool) async throws {
    let deadline = ContinuousClock.now.advanced(by: .seconds(3))
    while !(await condition()) {
        guard ContinuousClock.now < deadline else { throw ContractError.invalid("Panel invalidation did not settle") }
        try await Task.sleep(for: .milliseconds(2))
    }
}
private struct PanelRig: Sendable {
    let transport: PanelEventTransport
    let live: LiveHouse
    let views: PanelViews
    let consumer: Task<Void, Never>
    init(_ zones: [Zone], retry: Duration = .milliseconds(50)) throws {
        transport = PanelEventTransport(zones); views = PanelViews()
        let client = DaemonClient(endpoint: try DaemonEndpoint("https://daemon.example.test"), transport: transport)
        live = LiveHouse(client: client, timing: .init(coalesce: .milliseconds(30), firstRetry: retry, maxRetry: retry))
        let live = live, views = views
        consumer = Task { for await view in live.updates { await views.append(view) } }
    }
    func start() async throws {
        await live.resume()
        try await panelEventually { await views.latest.map { $0.isFresh && $0.panelRevision > 0 } == true }
    }
    func close() async { await live.stop(); await transport.release(); consumer.cancel(); await consumer.value }
}
private func withPanelRig(_ zones: [Zone] = [panelZone()], retry: Duration = .milliseconds(50),
                          _ body: (PanelRig) async throws -> Void) async throws {
    let rig = try PanelRig(zones, retry: retry)
    do { try await body(rig); await rig.close() }
    catch { await rig.close(); throw error }
}

@Test func panelInvalidationRecognizesOnlyAllowedWriteToolsNotTheirDetails() throws {
    for tool in ["save_scene", "delete_scene", "add_schedule", "remove_schedule", "pause_schedule",
                 "resume_schedule", "set_sleep_timer", "undo", "undo_last"] {
        for intent in [tool, "\(tool): fixture: another colon"] {
            #expect(LiveHouse.invalidatesPanels(try panelAction(intent)))
            #expect(LiveHouse.invalidatesPanels(try panelAction(intent, decision: "clamp: policy")))
            #expect(!LiveHouse.invalidatesPanels(try panelAction(intent, decision: "deny: policy")))
        }
    }
    for intent in ["", ":", "list_scenes", "get_sleep_timer", "list_schedules", "recent_actions",
                   "play: save_scene", "unknown: add_schedule", "save_scene_extra", "schedule"] {
        #expect(!LiveHouse.invalidatesPanels(try panelAction(intent)))
    }
    for data in ["null", "[]", "{}", "{", "{\"intent\":\"save_scene\"}",
                 "{\"intent\":12,\"decision\":\"allow\"}", "{\"intent\":\"save_scene\",\"decision\":null}"] {
        #expect(!LiveHouse.invalidatesPanels(data))
    }
    #expect(!LiveHouse.invalidatesPanels(try panelAction("save_scene", decision: "allow extra")))
}

@Test func panelActionBurstsPublishOneRevisionWithoutReadingSpeakers() async throws {
    try await withPanelRig { rig in
        try await rig.start()
        let initial = try #require(await rig.views.latest)
        let reads = await rig.transport.requests.count
        await rig.transport.emit("action.logged", data: try panelAction("save_scene: Evening"), count: 100)
        try await panelEventually { await rig.views.latest?.panelRevision == initial.panelRevision + 1 }
        try await Task.sleep(for: .milliseconds(90))
        let current = try #require(await rig.views.latest)
        #expect(current.panelRevision == initial.panelRevision + 1)
        #expect(current.isFresh && current.isFresh(for: panelZone().id))
        #expect(current.snapshot?.stateObservedAt == initial.snapshot?.stateObservedAt)
        #expect(await rig.transport.requests.count == reads)
        #expect(await rig.transport.streams.count == 1)
    }
}

@Test func panelReadDenialsCannotCreateAnEventRefreshLoop() async throws {
    try await withPanelRig { rig in
        try await rig.start()
        let initial = try #require(await rig.views.latest)
        let reads = await rig.transport.requests.count
        await rig.transport.emit("action.logged", data: try panelAction("list_scenes", decision: "deny: policy"), count: 100)
        await rig.transport.emit("action.logged", data: try panelAction("save_scene", decision: "deny: policy"), count: 100)
        await rig.transport.emit("action.logged", data: "{}", count: 100)
        await rig.transport.emit("future.event", count: 100)
        try await Task.sleep(for: .milliseconds(100))
        #expect(await rig.views.latest?.panelRevision == initial.panelRevision)
        #expect(await rig.transport.requests.count == reads)
    }
}

@Test func panelActionsAndResetReachAnEmptyHouse() async throws {
    try await withPanelRig([]) { rig in
        try await rig.start()
        let initial = try #require(await rig.views.latest?.panelRevision)
        await rig.transport.emit("action.logged", data: try panelAction("add_schedule: #2"))
        try await panelEventually { await rig.views.latest?.panelRevision == initial + 1 }
        #expect(await rig.views.latest?.snapshot?.zones.isEmpty == true)
        await rig.transport.emit("events.reset")
        try await panelEventually { await rig.views.latest.map { $0.isFresh && $0.panelRevision == initial + 2 } == true }
        #expect(await rig.views.latest?.snapshot?.zones.isEmpty == true)
        #expect(await rig.transport.requests.allSatisfy { $0.url?.path == "/zones" && ($0.httpMethod ?? "GET") == "GET" })
    }
}

@Test func panelActionsWorkWithoutAReadableTopology() async throws {
    try await withPanelRig([], retry: .seconds(10)) { rig in
        await rig.transport.fail(); await rig.live.resume()
        try await panelEventually { await rig.views.latest.map { $0.phase == .retrying && $0.panelRevision > 0 } == true }
        let initial = try #require(await rig.views.latest)
        let reads = await rig.transport.requests.count
        await rig.transport.emit("action.logged", data: try panelAction("delete_scene: Evening"))
        try await panelEventually { await rig.views.latest?.panelRevision == initial.panelRevision + 1 }
        let current = try #require(await rig.views.latest)
        #expect(current.snapshot == nil && current.freshRooms.isEmpty && !current.isFresh)
        #expect(current.phase == .retrying && current.problem == initial.problem)
        #expect(await rig.transport.requests.count == reads)
    }
}

@Test func panelReconnectInvalidatesCatalogsEvenWithUnchangedPlayback() async throws {
    try await withPanelRig { rig in
        try await rig.start()
        let initial = try #require(await rig.views.latest)
        await rig.transport.disconnect()
        try await panelEventually { await rig.transport.streams.count == 2 }
        try await panelEventually { await rig.views.latest.map { $0.isFresh && $0.panelRevision > initial.panelRevision } == true }
        let current = try #require(await rig.views.latest)
        #expect(current.snapshot?.states == initial.snapshot?.states)
        #expect(await rig.transport.requests.allSatisfy { ($0.httpMethod ?? "GET") == "GET" })
    }
}

@Test func panelPublicationKeepsInFlightRoomReadsStale() async throws {
    try await withPanelRig { rig in
        try await rig.start()
        let initial = try #require(await rig.views.latest?.panelRevision)
        await rig.transport.hold()
        await rig.transport.emit("zone.state", data: "{\"room\":\"Studio\"}")
        try await panelEventually { await rig.transport.blocked }
        await rig.transport.emit("action.logged", data: try panelAction("set_sleep_timer: Studio"))
        try await panelEventually { await rig.views.latest?.panelRevision == initial + 1 }
        let reading = try #require(await rig.views.latest)
        #expect(!reading.isFresh && !reading.isFresh(for: panelZone().id))
        #expect(reading.phase == .refreshing)
        await rig.transport.release()
        try await panelEventually { await rig.views.latest?.isFresh == true }
        #expect(await rig.views.latest?.panelRevision == initial + 1)
    }
}

@Test func panelSuspensionCancelsPendingInvalidationAndResumeGetsANewOne() async throws {
    try await withPanelRig { rig in
        try await rig.start()
        let initial = try #require(await rig.views.latest?.panelRevision)
        await rig.transport.emit("action.logged", data: try panelAction("save_scene: Evening"))
        await rig.live.suspend()
        try await panelEventually { await rig.views.latest?.phase == .suspended }
        try await Task.sleep(for: .milliseconds(70))
        #expect(await rig.views.latest?.panelRevision == initial)
        await rig.live.resume()
        try await panelEventually { await rig.views.latest.map { $0.isFresh && $0.panelRevision == initial + 1 } == true }
        await rig.live.stop()
        let reads = await rig.transport.requests.count
        await rig.transport.emit("action.logged", data: try panelAction("save_scene: After stop"))
        await rig.live.resume()
        try await Task.sleep(for: .milliseconds(70))
        #expect(await rig.transport.requests.count == reads)
        #expect(await rig.views.latest?.phase == .suspended)
    }
}

@Test func panelRevisionSurvivesSkippedBufferedViews() async throws {
    let transport = PanelEventTransport([panelZone()])
    let client = DaemonClient(endpoint: try DaemonEndpoint("https://daemon.example.test"), transport: transport)
    let live = LiveHouse(client: client, timing: .init(coalesce: .milliseconds(10), firstRetry: .seconds(1), maxRetry: .seconds(1)))
    // Deliberately leave the one-element display buffer unconsumed.
    await live.resume()
    try await panelEventually { await transport.requests.count >= 2 }
    try await Task.sleep(for: .milliseconds(50))
    await transport.emit("action.logged", data: try panelAction("pause_schedule: #2"))
    try await Task.sleep(for: .milliseconds(50))
    await transport.emit("zone.state", data: "{\"room\":\"Studio\"}")
    try await Task.sleep(for: .milliseconds(50))
    var iterator = live.updates.makeAsyncIterator()
    let view = await iterator.next()
    #expect(view?.isFresh == true && view?.panelRevision == 2)
    await live.stop()
}

@Test func panelObservationDetectsUnchangedPlaybackAfterSkippingStaleViews() {
    let zone = panelZone(), other = panelZone("TEST-B")
    let t0 = Date(timeIntervalSince1970: 100), t1 = Date(timeIntervalSince1970: 101)
    let states = [zone.id: panelState(zone), other.id: panelState(other)]
    func view(_ dates: [RoomID: Date], fresh: Set<RoomID>? = nil) -> LiveHouseView {
        LiveHouseView(snapshot: HouseSnapshot(zones: [zone, other], states: states, failures: [:],
                                             observedAt: t1, stateObservedAt: dates),
                      phase: .live, problem: nil, freshRooms: fresh)
    }
    let initial = view([zone.id: t0, other.id: t0])
    let anotherRoom = view([zone.id: t0, other.id: t1])
    let selectedRoom = view([zone.id: t1, other.id: t0])
    #expect(!initial.hasNewPlaybackObservation(for: zone.id, since: initial))
    #expect(!anotherRoom.hasNewPlaybackObservation(for: zone.id, since: initial))
    #expect(selectedRoom.hasNewPlaybackObservation(for: zone.id, since: initial))
    #expect(!selectedRoom.hasNewPlaybackObservation(for: other.id, since: initial))
    #expect(!view([zone.id: t1], fresh: []).hasNewPlaybackObservation(for: zone.id, since: initial))
    #expect(initial.hasNewPlaybackObservation(for: zone.id, since: view([zone.id: t0], fresh: [])))
    #expect(initial.hasNewPlaybackObservation(for: zone.id, since: nil))
    #expect(!initial.hasNewPlaybackObservation(for: RoomID(household: "absent", room: "Studio"), since: nil))
}

@Test(arguments: ["add_to_queue", "remove_from_queue", "move_in_queue"])
func panelQueueEditsInvalidateWithoutAPlaybackChange(_ tool: String) async throws {
    try await withPanelRig { rig in
        try await rig.start()
        let initial = try #require(await rig.views.latest)
        let reads = await rig.transport.requests.count
        await rig.transport.emit("action.logged", data: try panelAction("\(tool): Studio"))
        try await panelEventually { await rig.views.latest?.panelRevision == initial.panelRevision + 1 }
        let current = try #require(await rig.views.latest)
        #expect(current.snapshot?.states == initial.snapshot?.states)
        #expect(current.snapshot?.stateObservedAt == initial.snapshot?.stateObservedAt)
        #expect(current.isFresh)
        #expect(await rig.transport.requests.count == reads)
        #expect(!LiveHouse.invalidatesPanels(try panelAction("\(tool): Studio", decision: "deny: policy")))
        #expect(!LiveHouse.invalidatesPanels(try panelAction("get_queue: \(tool)", decision: "deny: policy")))
    }
}
