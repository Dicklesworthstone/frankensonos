import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

private actor LiveFixture: DaemonTransport {
    var groups: [Zone]
    var failing = Set<String>(), topologyFails = false
    var volume = 20
    var paths: [String] = [], methods: [String] = []
    var headers: [String?] = []
    var streams: [AsyncThrowingStream<EventSignal, Error>.Continuation] = []
    var closed = Set<Int>()
    var active = 0, peak = 0
    var hold = false, autoConnect = true
    private var waiters: [CheckedContinuation<Void, Never>] = []

    init(_ groups: [Zone]) { self.groups = groups }
    func configure(failing: Set<String> = [], topologyFails: Bool = false) {
        self.failing = failing; self.topologyFails = topologyFails
    }
    func change(_ groups: [Zone]) { self.groups = groups }
    func setVolume(_ value: Int) { volume = value }
    func block() { hold = true }
    func connectAutomatically(_ value: Bool) { autoConnect = value }
    func release() {
        hold = false
        let pending = waiters; waiters.removeAll()
        for waiter in pending { waiter.resume() }
    }
    func send(_ request: URLRequest) async throws -> HTTPReply {
        let path = request.url!.path
        paths.append(path); methods.append(request.httpMethod ?? "GET")
        let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
        if path == "/zones" {
            if topologyFails { throw ClientError.http(503, "Test topology unavailable") }
            return HTTPReply(data: try encoder.encode(groups), status: 200)
        }
        let address = String(path.dropFirst("/zones/".count).dropLast("/state".count))
        active += 1; peak = max(peak, active)
        defer { active -= 1 }
        // Intentionally ignore cancellation until released: exercise the
        // session's generation check and non-overlapping read slot.
        if hold { await withCheckedContinuation { waiters.append($0) } }
        if failing.contains(address) { throw ClientError.http(503, "Test room unavailable") }
        guard let zone = groups.first(where: { z in z.members.contains { "\($0)@\(z.household)" == address } }) else {
            throw ClientError.unreadable
        }
        let state = ZoneState(zone: zone, transportState: "paused", volume: volume, mute: false,
                              groupVolume: volume, track: nil, shuffle: false, repeat: "off")
        return HTTPReply(data: try encoder.encode(state), status: 200)
    }
    nonisolated func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> {
        let pair = AsyncThrowingStream<EventSignal, Error>.makeStream()
        Task { await register(request, pair.continuation) }
        return pair.stream
    }
    private func register(_ request: URLRequest, _ continuation: AsyncThrowingStream<EventSignal, Error>.Continuation) {
        let id = streams.count
        streams.append(continuation); headers.append(request.value(forHTTPHeaderField: "Last-Event-ID"))
        continuation.onTermination = { @Sendable [weak self] _ in Task { await self?.ended(id) } }
        if autoConnect { continuation.yield(.connected) }
    }
    private func ended(_ id: Int) { closed.insert(id) }
    func emit(_ signal: EventSignal, stream: Int? = nil) { streams[stream ?? (streams.count - 1)].yield(signal) }
    func event(_ kind: String, _ data: String = "{}", id: String? = nil) {
        emit(.event(ServerEvent(id: id, kind: kind, data: data)))
    }
    func burst(_ room: String, count: Int) {
        for n in 0..<count { event("zone.state", "{\"room\":\"\(room)\"}", id: String(n + 1)) }
    }
    func disconnect(_ error: ClientError? = .unreadable) {
        if let error { streams.last!.finish(throwing: error) } else { streams.last!.finish() }
    }
    func reads(_ path: String) -> Int { paths.filter { $0 == path }.count }
}

private actor LiveViews {
    var values: [LiveHouseView] = []
    var latest: LiveHouseView? { values.last }
    func append(_ value: LiveHouseView) { values.append(value) }
}
private func liveZone(_ home: String = "S2", _ rooms: [String] = ["A", "B"]) -> Zone {
    Zone(household: home, coordinatorRoom: rooms[0], members: rooms, transportState: "paused")
}
private struct LiveRig {
    let fixture: LiveFixture, session: LiveHouse, views: LiveViews
    let consumer: Task<Void, Never>
    init(_ groups: [Zone] = [liveZone()]) throws {
        fixture = LiveFixture(groups); views = LiveViews()
        let client = DaemonClient(endpoint: try DaemonEndpoint("https://example.invalid"), transport: fixture)
        session = LiveHouse(client: client, timing: .init(coalesce: .milliseconds(20), firstRetry: .milliseconds(40), maxRetry: .milliseconds(160)))
        let session = session, views = views
        consumer = Task { for await value in session.updates { await views.append(value) } }
    }
    func start() async throws {
        await session.resume()
        try await eventually { await views.latest?.isFresh == true }
    }
    func close() async {
        await session.stop(); await fixture.release(); consumer.cancel(); await consumer.value
    }
}
private func eventually(_ condition: @escaping @Sendable () async -> Bool) async throws {
    let clock = ContinuousClock(), end = clock.now + .seconds(3)
    while clock.now < end {
        if await condition() { return }
        try await Task.sleep(for: .milliseconds(2))
    }
    throw ContractError.invalid("Test observation did not arrive before the deadline")
}

@Test func liveStartsIdempotentlyAndDoesNotPollAQuietHealthyConnection() async throws {
    let rig = try LiveRig()
    try await rig.start()
    await rig.session.resume(); await rig.session.resume()
    let count = await rig.fixture.paths.count
    try await Task.sleep(for: .milliseconds(120))
    #expect(await rig.fixture.streams.count == 1)
    #expect(await rig.fixture.paths.count == count)
    #expect(await rig.views.latest?.snapshot?.states.count == 2)
    await rig.close()
}

@Test func liveCoalescesBurstsAndRefreshesEverySameNamedHousehold() async throws {
    let rig = try LiveRig([liveZone("S1", ["Den"]), liveZone("S2", ["Den", "B"])])
    try await rig.start()
    let initial = await rig.fixture.paths.count
    await rig.fixture.setVolume(31); await rig.fixture.burst("Den", count: 30)
    try await eventually {
        let view = await rig.views.latest
        return view?.isFresh == true && view?.snapshot?.states.values.allSatisfy { $0.volume == 31 } == true
    }
    #expect(await rig.fixture.paths.count == initial + 3)
    #expect(await rig.fixture.reads("/zones") == 1)
    await rig.close()
}

@Test func liveReconnectsWithCursorAndRebuildsTheSnapshotWithoutWrites() async throws {
    let rig = try LiveRig()
    try await rig.start()
    await rig.fixture.event("zone.state", "{\"room\":\"A\"}", id: "17")
    try await Task.sleep(for: .milliseconds(30))
    await rig.fixture.disconnect()
    try await eventually { await rig.fixture.streams.count == 2 }
    try await eventually { await rig.views.latest?.isFresh == true }
    #expect(await rig.fixture.headers[1] == "17")
    #expect(await rig.fixture.reads("/zones") >= 2)
    #expect(await rig.fixture.methods.allSatisfy { $0 == "GET" })
    #expect(await rig.views.values.contains { $0.phase == .retrying && $0.snapshot != nil })
    await rig.close()
}

@Test func liveCleanEndOfStreamAlsoReconnects() async throws {
    let rig = try LiveRig()
    try await rig.start(); await rig.fixture.disconnect(nil)
    try await eventually { await rig.fixture.streams.count == 2 }
    try await eventually { await rig.views.latest?.isFresh == true }
    await rig.close()
}

@Test func liveResetDiscardsTheCursorAndReconcilesChangedTopology() async throws {
    let rig = try LiveRig()
    try await rig.start()
    await rig.fixture.event("zone.state", "{\"room\":\"A\"}", id: "90")
    await rig.fixture.change([liveZone("S2", ["B"])]); await rig.fixture.event("events.reset", "{}", id: "91")
    try await eventually {
        let view = await rig.views.latest
        return view?.isFresh == true && view?.snapshot?.states.count == 1
    }
    #expect(await rig.views.latest?.snapshot?.states[RoomID(household: "S2", room: "A")] == nil)
    await rig.fixture.disconnect()
    try await eventually { await rig.fixture.streams.count == 2 }
    #expect(await rig.fixture.headers[1] == nil)
    await rig.close()
}

@Test func liveMalformedAndUnknownRoomEventsRequestFullSnapshots() async throws {
    let rig = try LiveRig()
    try await rig.start()
    let initial = await rig.fixture.reads("/zones")
    await rig.fixture.event("zone.state", "{")
    try await eventually {
        let checked = await rig.fixture.reads("/zones") > initial
        let fresh = await rig.views.latest?.isFresh == true
        return checked && fresh
    }
    let beforeUnknown = await rig.fixture.reads("/zones")
    await rig.fixture.event("zone.state", "{\"room\":\"New Room\"}")
    try await eventually {
        let checked = await rig.fixture.reads("/zones") > beforeUnknown
        let fresh = await rig.views.latest?.isFresh == true
        return checked && fresh
    }
    await rig.close()
}

@Test func liveRetriesFailedRoomsWithoutAnotherSpeakerEvent() async throws {
    let rig = try LiveRig()
    await rig.fixture.configure(failing: ["A@S2"]); await rig.session.resume()
    try await eventually { await rig.views.latest?.snapshot?.failures.count == 1 }
    #expect(await rig.views.latest?.snapshot?.states.count == 1)
    #expect(await rig.views.latest?.isFresh == false)
    await rig.fixture.configure()
    try await eventually { await rig.views.latest?.isFresh == true }
    #expect(await rig.views.latest?.snapshot?.states.count == 2)
    await rig.close()
}

@Test func liveTopologyFailureNeverPublishesAnEmptyHouse() async throws {
    let rig = try LiveRig()
    await rig.fixture.configure(topologyFails: true); await rig.session.resume()
    try await eventually { await rig.views.latest?.phase == .retrying }
    #expect(await rig.views.latest?.snapshot == nil)
    await rig.fixture.configure()
    try await eventually { await rig.views.latest?.isFresh == true }
    await rig.close()
}

@Test func liveEventsDuringAReadCauseOneLaterBatchNotAFalseFreshView() async throws {
    let rig = try LiveRig()
    try await rig.start()
    await rig.fixture.block(); await rig.fixture.event("zone.state", "{\"room\":\"A\"}")
    try await eventually { await rig.fixture.active == 2 }
    let before = await rig.fixture.paths.count
    await rig.fixture.burst("A", count: 30)
    try await Task.sleep(for: .milliseconds(30))
    #expect(await rig.fixture.paths.count == before)
    #expect(await rig.views.latest?.isFresh == false)
    await rig.fixture.setVolume(39); await rig.fixture.release()
    try await eventually {
        let checked = await rig.fixture.paths.count == before + 2
        let fresh = await rig.views.latest?.isFresh == true
        return checked && fresh
    }
    #expect(await rig.fixture.peak == 2)
    await rig.close()
}

@Test func liveSuspendAndRapidResumeWaitForOldReadsAndRejectTheirResults() async throws {
    let rig = try LiveRig()
    try await rig.start()
    await rig.fixture.block(); await rig.fixture.event("zone.state", "{\"room\":\"A\"}")
    try await eventually { await rig.fixture.active == 2 }
    let before = await rig.fixture.paths.count, mark = await rig.views.values.count
    await rig.session.suspend(); await rig.session.resume()
    try await Task.sleep(for: .milliseconds(40))
    #expect(await rig.fixture.paths.count == before)
    await rig.fixture.setVolume(44); await rig.fixture.release()
    try await eventually { await rig.views.latest?.isFresh == true }
    #expect(await rig.views.values.dropFirst(mark).filter(\.isFresh).allSatisfy {
        $0.snapshot?.states.values.allSatisfy { $0.volume == 44 } == true
    })
    #expect(await rig.fixture.peak == 2)
    await rig.close()
}

@Test func liveConsumerCancellationClosesTheConnectionAndStopsReads() async throws {
    let rig = try LiveRig()
    try await rig.start(); rig.consumer.cancel(); await rig.consumer.value
    try await eventually { await rig.fixture.closed.contains(0) }
    let count = await rig.fixture.paths.count
    await rig.fixture.event("zone.state", "{\"room\":\"A\"}")
    try await Task.sleep(for: .milliseconds(80))
    #expect(await rig.fixture.paths.count == count)
    await rig.close()
}

@Test func liveOverflowDropsCursorAndRecoversFromAFullRead() async throws {
    let rig = try LiveRig()
    try await rig.start()
    await rig.fixture.event("zone.state", "{\"room\":\"A\"}", id: "8")
    try await Task.sleep(for: .milliseconds(30)); await rig.fixture.disconnect(.eventOverflow)
    try await eventually { await rig.fixture.streams.count == 2 }
    try await eventually { await rig.views.latest?.isFresh == true }
    #expect(await rig.fixture.headers[1] == nil)
    await rig.close()
}

@Test func liveConnectionDuringAnInitialReadRequiresAnotherFullSnapshot() async throws {
    let rig = try LiveRig()
    await rig.fixture.connectAutomatically(false); await rig.fixture.block(); await rig.session.resume()
    try await eventually { await rig.fixture.active == 2 }
    await rig.fixture.emit(.connected); await rig.fixture.release()
    try await eventually { await rig.views.latest?.isFresh == true }
    #expect(await rig.fixture.reads("/zones") == 2)
    await rig.close()
}

@Test func liveNotificationsDoNotBypassFailedReadBackoff() async throws {
    let rig = try LiveRig()
    await rig.fixture.configure(failing: ["A@S2"]); await rig.session.resume()
    try await eventually { await rig.views.latest?.snapshot?.failures.count == 1 }
    let before = await rig.fixture.paths.count
    await rig.fixture.burst("A", count: 500)
    try await Task.sleep(for: .milliseconds(25))
    #expect(await rig.fixture.paths.count == before)
    #expect(await rig.views.latest?.isFresh == false)
    await rig.fixture.configure()
    try await eventually { await rig.views.latest?.isFresh == true }
    await rig.close()
}

@Test func liveLargeInvalidationBurstsCollapseToOneFullRead() async throws {
    let rig = try LiveRig()
    try await rig.start()
    let before = await rig.fixture.reads("/zones")
    for n in 0..<100 { await rig.fixture.event("zone.state", "{\"room\":\"New\(n)\"}") }
    try await eventually {
        let reads = await rig.fixture.reads("/zones")
        let view = await rig.views.latest
        return reads > before && view?.isFresh == true
    }
    #expect(await rig.fixture.reads("/zones") == before + 1)
    await rig.close()
}

@Test func liveStopIsTerminalAndCannotReplayWorkOnResume() async throws {
    let rig = try LiveRig()
    try await rig.start(); await rig.session.stop()
    let before = await rig.fixture.paths.count, connections = await rig.fixture.streams.count
    await rig.session.resume(); await rig.session.refresh()
    try await Task.sleep(for: .milliseconds(80))
    #expect(await rig.fixture.paths.count == before)
    #expect(await rig.fixture.streams.count == connections)
    await rig.close()
}
