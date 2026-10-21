import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

// Shared with RemoteFreshnessTests: injected at the existing HTTP/SSE boundary.
actor FreshRoomTransport: DaemonTransport {
    var groups: [Zone]
    var failed: Set<RoomID> = []
    var topologyFailure = false
    var version = 1
    var requests: [URLRequest] = []
    var streams: [AsyncThrowingStream<EventSignal, Error>.Continuation] = []
    var held: Set<String> = []
    var waiting: [String: [CheckedContinuation<Void, Never>]] = [:]
    var automaticallyConnect = true
    init(_ groups: [Zone]) { self.groups = groups }
    func fail(_ rooms: Set<RoomID>) { failed = rooms }
    func failTopology(_ fail: Bool) { topologyFailure = fail }
    func change(_ groups: [Zone]) { self.groups = groups }
    func setVersion(_ value: Int) { version = value }
    func hold(_ path: String) { held.insert(path) }
    func autoConnect(_ value: Bool) { automaticallyConnect = value }
    func release(_ path: String) {
        held.remove(path)
        let pending = waiting.removeValue(forKey: path) ?? []
        for waiter in pending { waiter.resume() }
    }
    func releaseAll() { for path in Array(held) { release(path) } }
    func reads(_ path: String) -> Int { requests.filter { $0.url?.path == path }.count }
    func send(_ request: URLRequest) async throws -> HTTPReply {
        requests.append(request)
        guard request.httpMethod == nil || request.httpMethod == "GET" else {
            throw ContractError.invalid("Freshness must never send a playback write")
        }
        let path = request.url!.path
        let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
        let data: Data
        let failedRead: Bool
        if path == "/zones" {
            data = try encoder.encode(groups); failedRead = topologyFailure
        } else {
            guard let zone = groups.first(where: { group in
                group.rooms.contains { path.hasPrefix("/zones/" + $0.address + "/") }
            }) else { throw ClientError.unreadable }
            let room = try #require(zone.rooms.first { path.hasPrefix("/zones/" + $0.address + "/") })
            failedRead = failed.contains(room)
            if path.hasSuffix("/queue") {
                data = try JSONSerialization.data(withJSONObject: ["total": 1, "revision": String(version),
                    "items": [["position": 1, "title": "Pick \(version)", "uri": "fixture:\(version)"]]])
            } else {
                let track = Track(uri: "fixture:\(version)", title: "Pick \(version)", creator: nil,
                                  album: nil, durationSecs: 300, positionSecs: 10, queuePosition: 1)
                data = try encoder.encode(ZoneState(zone: zone, transportState: "playing", volume: 20,
                    mute: false, groupVolume: 20, track: track, shuffle: false, repeat: "off"))
            }
        }
        // Capture the reply before waiting; a later event needs another read.
        if held.contains(path) { await withCheckedContinuation { waiting[path, default: []].append($0) } }
        if failedRead { throw ClientError.http(503, "Fixture room or topology unavailable") }
        return HTTPReply(data: data, status: 200)
    }
    nonisolated func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> {
        let pair = AsyncThrowingStream<EventSignal, Error>.makeStream()
        Task { await register(pair.continuation) }
        return pair.stream
    }
    private func register(_ feed: AsyncThrowingStream<EventSignal, Error>.Continuation) {
        streams.append(feed)
        if automaticallyConnect { feed.yield(.connected) }
    }
    func connect() { streams.last?.yield(.connected) }
    func event(_ kind: String, room: String? = nil) throws {
        let data = try JSONSerialization.data(withJSONObject: room.map { ["room": $0] } ?? [:])
        streams.last?.yield(.event(ServerEvent(id: nil, kind: kind, data: String(decoding: data, as: UTF8.self))))
    }
    func disconnect() { streams.last?.finish(throwing: ClientError.unreadable) }
}
func freshZone(_ room: String, home: String = "S2", members: [String]? = nil) -> Zone {
    Zone(household: home, coordinatorRoom: room, members: members ?? [room], transportState: "playing")
}
func freshClient(_ transport: FreshRoomTransport) throws -> DaemonClient {
    DaemonClient(endpoint: try DaemonEndpoint("https://example.invalid"), transport: transport)
}
func freshnessWait(_ condition: @Sendable () async -> Bool) async throws {
    let clock = ContinuousClock(), end = clock.now + .seconds(4)
    while clock.now < end {
        if await condition() { return }
        try await Task.sleep(for: .milliseconds(2))
    }
    throw ContractError.invalid("Room freshness test timed out")
}
private actor FreshViews {
    var all: [LiveHouseView] = []
    var last: LiveHouseView? { all.last }
    func accept(_ view: LiveHouseView) { all.append(view) }
}
private struct FreshRig {
    let transport: FreshRoomTransport
    let live: LiveHouse
    let views: FreshViews
    let consumer: Task<Void, Never>
    init(_ zones: [Zone] = [freshZone("A"), freshZone("B")], retry: Duration = .seconds(30)) throws {
        transport = FreshRoomTransport(zones); views = FreshViews()
        live = LiveHouse(client: try freshClient(transport), timing: .init(
            coalesce: .milliseconds(10), firstRetry: retry, maxRetry: .seconds(30)))
        let live = live, views = views
        consumer = Task { for await value in live.updates { await views.accept(value) } }
    }
    func start() async throws {
        await live.resume()
        try await freshnessWait {
            let view = await views.last
            return view?.snapshot != nil && view?.isConnected == true
        }
    }
    func close() async {
        await live.stop(); await transport.releaseAll(); consumer.cancel(); await consumer.value
    }
}

@Test func roomFreshnessSeparatesUnavailableMembersFromHealthyPlayback() async throws {
    let rig = try FreshRig([freshZone("A", members: ["A", "Offline"]), freshZone("B")])
    let offline = RoomID(household: "S2", room: "Offline")
    await rig.transport.fail([offline]); try await rig.start()
    let view = try #require(await rig.views.last)
    #expect(!view.isFresh && view.isConnected)
    #expect(view.isFresh(for: freshZone("A").id) && view.isFresh(for: freshZone("B").id))
    #expect(!view.isFresh(for: offline))
    #expect(view.snapshot?.failures.count == 1)
    #expect(await rig.transport.requests.allSatisfy { $0.httpMethod == "GET" })
    await rig.close()
}

@Test func roomFreshnessKeepsConsumedInvalidationsStaleUntilTheirReadFinishes() async throws {
    let rig = try FreshRig(); try await rig.start()
    let a = freshZone("A").id, b = freshZone("B").id
    let aPath = "/zones/A@S2/state", bPath = "/zones/B@S2/state"
    await rig.transport.hold(aPath); await rig.transport.hold(bPath)
    try await rig.transport.event("zone.state", room: "A")
    try await freshnessWait { await rig.transport.waiting[aPath]?.count == 1 }
    #expect(await rig.views.last?.isFresh(for: b) == true)
    try await rig.transport.event("zone.state", room: "B")
    try await freshnessWait { await rig.views.last?.freshRooms.isEmpty == true }
    // B's notification publishes while A's consumed invalidation is in flight.
    #expect(await rig.views.last?.isFresh(for: a) == false)
    await rig.transport.release(aPath)
    try await freshnessWait { await rig.transport.waiting[bPath]?.count == 1 }
    #expect(await rig.views.last?.isFresh(for: a) == true)
    #expect(await rig.views.last?.isFresh(for: b) == false)
    await rig.transport.release(bPath)
    try await freshnessWait { await rig.views.last?.isFresh == true }
    await rig.close()
}

@Test func roomFreshnessRetainsASecondEventDuringTheSameRoomRead() async throws {
    let rig = try FreshRig(); try await rig.start()
    let a = freshZone("A").id, path = "/zones/A@S2/state"
    await rig.transport.hold(path)
    try await rig.transport.event("zone.state", room: "A")
    try await freshnessWait { await rig.transport.waiting[path]?.count == 1 }
    let mark = await rig.views.all.count
    await rig.transport.setVersion(2)
    try await rig.transport.event("zone.state", room: "A")
    try await freshnessWait { await rig.views.all.count > mark }
    await rig.transport.release(path)
    try await freshnessWait { await rig.views.last?.isFresh(for: a) == true }
    let newViews = await rig.views.all.dropFirst(mark)
    #expect(newViews.filter { $0.isFresh(for: a) }.allSatisfy { $0.snapshot?.states[a]?.track?.uri == "fixture:2" })
    #expect(await rig.transport.reads(path) == 3)
    await rig.close()
}

@Test func roomFreshnessInvalidatesAllNamesakesButNotOtherGroups() async throws {
    let rig = try FreshRig([freshZone("Den", home: "S1"), freshZone("Den", members: ["Den", "Hall"]), freshZone("Study")])
    try await rig.start()
    await rig.transport.hold("/zones/Den@S1/state")
    await rig.transport.hold("/zones/Den@S2/state")
    try await rig.transport.event("zone.state", room: "Den")
    try await freshnessWait { await rig.transport.waiting["/zones/Den@S1/state"]?.count == 1 }
    let view = try #require(await rig.views.last)
    #expect(!view.isFresh(for: freshZone("Den", home: "S1").id))
    #expect(!view.isFresh(for: freshZone("Den").id))
    #expect(!view.isFresh(for: RoomID(household: "S2", room: "Hall")))
    #expect(view.isFresh(for: freshZone("Study").id))
    await rig.transport.releaseAll()
    try await freshnessWait { await rig.views.last?.isFresh == true }
    await rig.close()
}

@Test func roomFreshnessFullInvalidationSurvivesAnOlderTargetedReply() async throws {
    let rig = try FreshRig(); try await rig.start()
    await rig.transport.hold("/zones/A@S2/state")
    try await rig.transport.event("zone.state", room: "A")
    try await freshnessWait { await rig.transport.waiting["/zones/A@S2/state"]?.count == 1 }
    await rig.transport.hold("/zones")
    try await rig.transport.event("topology.changed")
    try await freshnessWait { await rig.views.last?.freshRooms.isEmpty == true }
    await rig.transport.release("/zones/A@S2/state")
    try await freshnessWait { await rig.transport.waiting["/zones"]?.count == 1 }
    #expect(await rig.views.last?.freshRooms.isEmpty == true)
    await rig.transport.release("/zones")
    try await freshnessWait { await rig.views.last?.isFresh == true }
    await rig.close()
}

@Test func roomFreshnessDropsAllAuthorityOnDisconnectAndWaitsForReconnectSnapshot() async throws {
    let rig = try FreshRig(retry: .milliseconds(60)); try await rig.start()
    await rig.transport.autoConnect(false); await rig.transport.disconnect()
    try await freshnessWait { await rig.views.last?.isConnected == false }
    #expect(await rig.views.last?.freshRooms.isEmpty == true)
    try await freshnessWait { await rig.transport.streams.count == 2 }
    await rig.transport.hold("/zones")
    await rig.transport.connect()
    try await freshnessWait { await rig.transport.waiting["/zones"]?.count == 1 }
    #expect(await rig.views.last?.isConnected == true)
    #expect(await rig.views.last?.freshRooms.isEmpty == true)
    await rig.transport.release("/zones")
    try await freshnessWait { await rig.views.last?.isFresh == true }
    await rig.close()
}

@Test func roomFreshnessOutageRecoveryPreservesHealthyObservationClocks() async throws {
    let rig = try FreshRig(retry: .milliseconds(80))
    let a = freshZone("A").id, b = freshZone("B").id
    await rig.transport.fail([a]); try await rig.start()
    let before = try #require(await rig.views.last?.snapshot?.stateObservedAt[b])
    await rig.transport.hold("/zones/A@S2/state")
    try await freshnessWait { await rig.transport.waiting["/zones/A@S2/state"]?.count == 1 }
    // Recovery of A must not prevent B's known-good clock from advancing.
    #expect(await rig.views.last?.isFresh(for: b) == true)
    await rig.transport.fail([]); await rig.transport.release("/zones/A@S2/state")
    try await freshnessWait { await rig.views.last?.isFresh == true }
    #expect(await rig.views.last?.snapshot?.stateObservedAt[b] == before)
    #expect(await rig.transport.reads("/zones/B@S2/state") == 1)
    await rig.close()
}

@Test func roomFreshnessTopologyReadFailureNeverRestoresCachedFreshFlags() async throws {
    let rig = try FreshRig(); try await rig.start()
    await rig.transport.failTopology(true)
    try await rig.transport.event("topology.changed")
    try await freshnessWait { await rig.views.last?.problem != nil }
    #expect(await rig.views.last?.freshRooms.isEmpty == true)
    #expect(await rig.views.last?.snapshot?.states.count == 2)
    await rig.transport.failTopology(false); await rig.live.refresh()
    try await freshnessWait { await rig.views.last?.isFresh == true }
    await rig.close()
}

@Test func roomFreshnessSuspensionAndUnknownEventsRequireNewObservations() async throws {
    let rig = try FreshRig(); try await rig.start()
    await rig.live.suspend()
    try await freshnessWait { await rig.views.last?.phase == .suspended }
    #expect(await rig.views.last?.freshRooms.isEmpty == true)
    await rig.transport.hold("/zones"); await rig.live.resume()
    try await freshnessWait { await rig.transport.waiting["/zones"]?.count == 1 }
    #expect(await rig.views.last?.freshRooms.isEmpty == true)
    await rig.transport.release("/zones")
    try await freshnessWait { await rig.views.last?.isFresh == true }
    await rig.transport.hold("/zones")
    try await rig.transport.event("zone.state", room: "New room")
    try await freshnessWait { await rig.transport.waiting["/zones"]?.count == 1 }
    #expect(await rig.views.last?.freshRooms.isEmpty == true)
    await rig.close()
}

@Test func roomFreshnessCannotInventMissingFailedOrUndatedObservations() throws {
    let a = freshZone("A"), b = freshZone("B")
    let state = ZoneState(zone: a, transportState: "playing", volume: 20, mute: false,
                          groupVolume: 20, track: nil, shuffle: false, repeat: "off")
    for snapshot in [
        HouseSnapshot(zones: [a, b], states: [a.id: state], failures: [a.id: "unavailable"], observedAt: Date()),
        HouseSnapshot(zones: [a, b], states: [a.id: state], failures: [:], observedAt: Date(), stateObservedAt: [:])
    ] {
        let view = LiveHouseView(snapshot: snapshot, phase: .retrying, problem: nil,
                                 isConnected: true, freshRooms: [a.id, b.id])
        #expect(view.freshRooms.isEmpty)
    }
    let snapshot = HouseSnapshot(zones: [a], states: [a.id: state], failures: [:], observedAt: Date())
    #expect(LiveHouseView(snapshot: snapshot, phase: .live, problem: nil).isFresh(for: a.id))
    #expect(!LiveHouseView(snapshot: snapshot, phase: .suspended, problem: nil,
                          isConnected: true, freshRooms: [a.id]).isFresh(for: a.id))
}
