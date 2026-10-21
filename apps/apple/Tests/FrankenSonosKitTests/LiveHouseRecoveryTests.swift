import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

private actor RoomOutage: DaemonTransport {
    var groups: [Zone]
    var failures: Set<RoomID>
    var topologyFails = false
    var volume = 20
    var paths: [String] = [], methods: [String] = []
    var streams: [AsyncThrowingStream<EventSignal, Error>.Continuation] = []
    var held = Set<RoomID>(), active = Set<RoomID>()
    var peak = 0
    var waiters: [CheckedContinuation<Void, Never>] = []
    init(_ groups: [Zone], failures: Set<RoomID>) { self.groups = groups; self.failures = failures }
    func change(groups: [Zone]? = nil, failures: Set<RoomID>? = nil, volume: Int? = nil, topologyFails: Bool? = nil) {
        if let groups { self.groups = groups }
        if let failures { self.failures = failures }
        if let volume { self.volume = volume }
        if let topologyFails { self.topologyFails = topologyFails }
    }
    func block(_ room: RoomID) { held.insert(room) }
    func release() {
        held = []
        let pending = waiters; waiters = []
        for waiter in pending { waiter.resume() }
    }
    func send(_ request: URLRequest) async throws -> HTTPReply {
        let path = request.url!.path
        paths.append(path); methods.append(request.httpMethod ?? "GET")
        let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
        if path == "/zones" {
            if topologyFails { throw ClientError.http(503, "Topology unavailable") }
            return HTTPReply(data: try encoder.encode(groups), status: 200)
        }
        guard let zone = groups.first(where: { $0.rooms.contains { path == "/zones/\($0.address)/state" } }),
              let id = zone.rooms.first(where: { path == "/zones/\($0.address)/state" }) else { throw ClientError.unreadable }
        // Capture the reply before a held read. Late replies must not acquire
        // the newer fixture's values and accidentally hide generation bugs.
        let state = ZoneState(zone: zone, transportState: "paused", volume: volume, mute: false,
                              groupVolume: volume, track: nil, shuffle: false, repeat: "off")
        let failed = failures.contains(id)
        #expect(!active.contains(id))
        active.insert(id); peak = max(peak, active.count)
        defer { active.remove(id) }
        if held.contains(id) { await withCheckedContinuation { waiters.append($0) } }
        if failed { throw ClientError.http(503, "Room unavailable") }
        return HTTPReply(data: try encoder.encode(state), status: 200)
    }
    nonisolated func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> {
        let pair = AsyncThrowingStream<EventSignal, Error>.makeStream()
        Task { await register(pair.continuation) }
        return pair.stream
    }
    private func register(_ stream: AsyncThrowingStream<EventSignal, Error>.Continuation) {
        streams.append(stream); stream.yield(.connected)
    }
    func event(_ kind: String, room: String? = nil) {
        let data = room.map { "{\"room\":\"\($0)\"}" } ?? "{}"
        streams.last?.yield(.event(ServerEvent(id: nil, kind: kind, data: data)))
    }
    func burst(_ room: String) { for _ in 0..<200 { event("zone.state", room: room) } }
    func reads(_ room: RoomID) -> Int { paths.filter { $0 == "/zones/\(room.address)/state" }.count }
    var topologies: Int { paths.filter { $0 == "/zones" }.count }
}
private actor OutageViews {
    var values: [LiveHouseView] = []
    var latest: LiveHouseView? { values.last }
    func append(_ view: LiveHouseView) { values.append(view) }
}
private func outageZone(_ home: String = "S2", _ rooms: [String]) -> Zone {
    Zone(household: home, coordinatorRoom: rooms[0], members: rooms, transportState: "paused")
}
private func outageRoom(_ name: String, _ home: String = "S2") -> RoomID { RoomID(household: home, room: name) }
private func outageEventually(_ check: @escaping @Sendable () async -> Bool) async throws {
    let clock = ContinuousClock(), end = clock.now + .seconds(2)
    while clock.now < end {
        if await check() { return }
        try await Task.sleep(for: .milliseconds(2))
    }
    throw ContractError.invalid("Live recovery observation did not arrive")
}
private struct OutageRig: Sendable {
    let transport: RoomOutage, live: LiveHouse, views: OutageViews
    let consumer: Task<Void, Never>
    init(groups: [Zone], failures: Set<RoomID>, retry: Duration) throws {
        transport = RoomOutage(groups, failures: failures); views = OutageViews()
        let client = DaemonClient(endpoint: try DaemonEndpoint("https://example.invalid"), transport: transport)
        live = LiveHouse(client: client, timing: .init(coalesce: .milliseconds(15), firstRetry: retry, maxRetry: retry * 4))
        let live = live, views = views
        consumer = Task { for await view in live.updates { await views.append(view) } }
    }
    func start() async throws {
        await live.resume()
        try await outageEventually { await views.latest?.snapshot?.failures.isEmpty == false }
    }
    func close() async {
        await live.stop(); await transport.release(); consumer.cancel(); await consumer.value
        #expect(await transport.methods.allSatisfy { $0 == "GET" })
    }
}
private func withOutage(groups: [Zone] = [outageZone("S2", ["A"]), outageZone("S2", ["B"])],
                        failed: Set<RoomID> = [outageRoom("A")], retry: Duration = .seconds(5),
                        _ test: @Sendable (OutageRig) async throws -> Void) async throws {
    let rig = try OutageRig(groups: groups, failures: failed, retry: retry)
    do { try await rig.start(); try await test(rig) }
    catch { await rig.close(); throw error }
    await rig.close()
}

@Test func liveOutageDoesNotBlockHealthyGroupEvents() async throws {
    try await withOutage { rig in
        let failedReads = await rig.transport.reads(outageRoom("A")), topologies = await rig.transport.topologies
        let before = await rig.views.latest?.snapshot?.stateObservedAt[outageRoom("B")]
        await rig.transport.change(volume: 42); await rig.transport.burst("B")
        try await outageEventually { await rig.views.latest?.snapshot?.states[outageRoom("B")]?.volume == 42 }
        #expect(await rig.transport.reads(outageRoom("A")) == failedReads)
        #expect(await rig.transport.topologies == topologies)
        let view = try #require(await rig.views.latest)
        #expect(!view.isFresh && view.snapshot?.failures[outageRoom("A")] != nil)
        #expect(view.snapshot?.states[outageRoom("A")] == nil)
        #expect(try #require(view.snapshot?.stateObservedAt[outageRoom("B")]) > #require(before))
    }
}

@Test func liveOutageDoesNotBlockHealthyMembersOfTheSameGroup() async throws {
    try await withOutage(groups: [outageZone("S2", ["A", "B"])]) { rig in
        let failedReads = await rig.transport.reads(outageRoom("A"))
        await rig.transport.change(volume: 31); await rig.transport.event("zone.state", room: "B")
        try await outageEventually { await rig.views.latest?.snapshot?.states[outageRoom("B")]?.volume == 31 }
        #expect(await rig.transport.reads(outageRoom("A")) == failedReads)
    }
}

@Test func liveOutageSeparatesSameNamedHouseholdsDuringBackoff() async throws {
    try await withOutage(groups: [outageZone("S1", ["Den"]), outageZone("S2", ["Den", "Hall"])],
                         failed: [outageRoom("Den", "S1")]) { rig in
        let failedReads = await rig.transport.reads(outageRoom("Den", "S1"))
        await rig.transport.change(volume: 38); await rig.transport.event("zone.state", room: "Den")
        try await outageEventually { await rig.views.latest?.snapshot?.states[outageRoom("Den")]?.volume == 38 }
        #expect(await rig.views.latest?.snapshot?.states[outageRoom("Hall")]?.volume == 38)
        #expect(await rig.transport.reads(outageRoom("Den", "S1")) == failedReads)
        #expect(await rig.views.latest?.snapshot?.failures[outageRoom("Den", "S1")] != nil)
    }
}

@Test func liveOutageAutomaticRetryLeavesHealthyRoomsAndClocksUntouched() async throws {
    try await withOutage(retry: .milliseconds(120)) { rig in
        let before = try #require(await rig.views.latest?.snapshot)
        let healthyReads = await rig.transport.reads(outageRoom("B"))
        await rig.transport.change(failures: [], volume: 49)
        try await outageEventually { await rig.views.latest?.isFresh == true }
        #expect(await rig.transport.reads(outageRoom("B")) == healthyReads)
        let after = try #require(await rig.views.latest?.snapshot)
        #expect(after.states[outageRoom("A")]?.volume == 49)
        #expect(after.states[outageRoom("B")] == before.states[outageRoom("B")])
        #expect(after.stateObservedAt[outageRoom("B")] == before.stateObservedAt[outageRoom("B")])
    }
}

@Test func liveOutageHealthyTrafficDoesNotPostponeRecoveryOrResetBackoff() async throws {
    try await withOutage(retry: .milliseconds(80)) { rig in
        for value in 21..<45 {
            await rig.transport.change(volume: value); await rig.transport.event("zone.state", room: "B")
            try await Task.sleep(for: .milliseconds(20))
        }
        // Automatic retries at roughly 80, 160, then 320 ms. Healthy reads
        // neither reset the 80 ms interval nor push the next retry away.
        let failedReads = await rig.transport.reads(outageRoom("A"))
        #expect((3...4).contains(failedReads))
        #expect(await rig.transport.reads(outageRoom("B")) > 5)
        await rig.transport.change(failures: [])
        try await outageEventually { await rig.views.latest?.isFresh == true }
    }
}

@Test func liveOutageNotificationsFromFailedRoomsCannotCauseAReadStorm() async throws {
    try await withOutage { rig in
        let before = await rig.transport.paths.count
        await rig.transport.burst("A")
        try await Task.sleep(for: .milliseconds(80))
        #expect(await rig.transport.paths.count == before)
        await rig.transport.change(volume: 27); await rig.transport.event("zone.state", room: "B")
        try await outageEventually { await rig.views.latest?.snapshot?.states[outageRoom("B")]?.volume == 27 }
    }
}

@Test func liveOutageRetryWaitsForAnActiveHealthyBatchThenRuns() async throws {
    try await withOutage(retry: .milliseconds(100)) { rig in
        await rig.transport.block(outageRoom("B")); await rig.transport.event("zone.state", room: "B")
        try await outageEventually { await rig.transport.active.contains(outageRoom("B")) }
        let before = await rig.transport.reads(outageRoom("A"))
        try await Task.sleep(for: .milliseconds(160))
        #expect(await rig.transport.reads(outageRoom("A")) == before)
        await rig.transport.change(failures: []); await rig.transport.release()
        try await outageEventually { await rig.views.latest?.isFresh == true }
        #expect(await rig.transport.reads(outageRoom("A")) == before + 1)
        #expect(await rig.transport.peak <= 4)
    }
}

@Test func liveOutageRecoveryFindsRegroupingWithoutAnotherEvent() async throws {
    try await withOutage(retry: .milliseconds(100)) { rig in
        await rig.transport.change(groups: [outageZone("S2", ["B", "C"])], failures: [], volume: 55)
        try await outageEventually { await rig.views.latest?.isFresh == true }
        let snapshot = try #require(await rig.views.latest?.snapshot)
        #expect(snapshot.zones.count == 1 && snapshot.zones[0].coordinatorRoom == "B")
        #expect(snapshot.states[outageRoom("A")] == nil && snapshot.failures.isEmpty)
        #expect(snapshot.states[outageRoom("B")]?.volume == 55 && snapshot.states[outageRoom("C")]?.volume == 55)
    }
}

@Test func liveOutageTopologyFailureBlocksTargetedReadsUntilAFullRecovery() async throws {
    try await withOutage(retry: .milliseconds(100)) { rig in
        await rig.transport.change(topologyFails: true)
        try await outageEventually { await rig.transport.topologies >= 2 }
        let before = await rig.transport.reads(outageRoom("B"))
        await rig.transport.change(volume: 40); await rig.transport.event("zone.state", room: "B")
        try await Task.sleep(for: .milliseconds(60))
        #expect(await rig.transport.reads(outageRoom("B")) == before)
        #expect(await rig.views.latest?.isFresh == false)
        await rig.transport.change(failures: [], topologyFails: false)
        try await outageEventually { await rig.views.latest?.isFresh == true }
        #expect(await rig.views.latest?.snapshot?.states[outageRoom("B")]?.volume == 40)
    }
}

@Test func liveOutageSuspensionDrainsRecoveryAndRejectsItsOldReply() async throws {
    try await withOutage(retry: .milliseconds(100)) { rig in
        await rig.transport.change(failures: []); await rig.transport.block(outageRoom("A"))
        try await outageEventually { await rig.transport.active.contains(outageRoom("A")) }
        let count = await rig.transport.paths.count
        await rig.live.suspend(); await rig.live.resume()
        await rig.transport.change(volume: 77)
        try await Task.sleep(for: .milliseconds(50))
        #expect(await rig.transport.paths.count == count)
        await rig.transport.release()
        try await outageEventually { await rig.views.latest?.isFresh == true }
        #expect(await rig.views.latest?.snapshot?.states.values.allSatisfy { $0.volume == 77 } == true)
        #expect(await rig.transport.peak <= 4)
    }
}

@Test func liveOutageEventsDuringRecoveryAreNotLostOrReportedFreshPrematurely() async throws {
    try await withOutage(retry: .milliseconds(100)) { rig in
        await rig.transport.change(failures: []); await rig.transport.block(outageRoom("A"))
        try await outageEventually { await rig.transport.active.contains(outageRoom("A")) }
        await rig.transport.change(volume: 64); await rig.transport.event("zone.state", room: "B")
        try await Task.sleep(for: .milliseconds(25))
        let mark = await rig.views.values.count
        await rig.transport.release()
        try await outageEventually {
            let view = await rig.views.latest
            return view?.isFresh == true && view?.snapshot?.states[outageRoom("B")]?.volume == 64
        }
        #expect(await rig.views.values.dropFirst(mark).filter(\.isFresh).allSatisfy {
            $0.snapshot?.states[outageRoom("B")]?.volume == 64
        })
    }
}

@Test func liveOutageExplicitRefreshCanRecoverBeforeTheBackoffDeadline() async throws {
    try await withOutage { rig in
        await rig.transport.change(failures: [], volume: 35); await rig.live.refresh()
        try await outageEventually { await rig.views.latest?.isFresh == true }
        #expect(await rig.views.latest?.snapshot?.states.values.allSatisfy { $0.volume == 35 } == true)
        let count = await rig.transport.paths.count
        try await Task.sleep(for: .milliseconds(100))
        #expect(await rig.transport.paths.count == count)
    }
}

@Test func liveOutageCoordinatorNotificationsStillRefreshHealthyGroupMembers() async throws {
    try await withOutage(groups: [outageZone("S2", ["A", "B"])]) { rig in
        let failedReads = await rig.transport.reads(outageRoom("A"))
        await rig.transport.change(volume: 46); await rig.transport.burst("A")
        try await outageEventually { await rig.views.latest?.snapshot?.states[outageRoom("B")]?.volume == 46 }
        #expect(await rig.transport.reads(outageRoom("A")) == failedReads)
        #expect(await rig.views.latest?.snapshot?.states[outageRoom("A")] == nil)
    }
}
