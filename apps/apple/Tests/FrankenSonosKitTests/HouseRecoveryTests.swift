import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

private actor RecoveryFixture: DaemonTransport {
    var groups: [Zone]
    var failing = Set<RoomID>()
    var wrongGroup = Set<RoomID>()
    var topologyFails = false
    var paths: [String] = []
    var volume = 20
    var delay: Duration = .zero
    var active = 0, peak = 0
    init(_ groups: [Zone]) { self.groups = groups }
    func configure(groups: [Zone]? = nil, failing: Set<RoomID> = [], wrongGroup: Set<RoomID> = [],
                   topologyFails: Bool = false, volume: Int = 20, delay: Duration = .zero) {
        if let groups { self.groups = groups }
        self.failing = failing; self.wrongGroup = wrongGroup; self.topologyFails = topologyFails
        self.volume = volume; self.delay = delay
    }
    func clearPaths() { paths = [] }
    func send(_ request: URLRequest) async throws -> HTTPReply {
        #expect(request.httpMethod == nil || request.httpMethod == "GET")
        let path = request.url!.path
        paths.append(path)
        let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
        if path == "/zones" {
            if topologyFails { throw ClientError.http(503, "Topology unavailable") }
            return HTTPReply(data: try encoder.encode(groups), status: 200)
        }
        active += 1; peak = max(peak, active)
        defer { active -= 1 }
        try await Task.sleep(for: delay)
        guard let zone = groups.first(where: { zone in zone.rooms.contains { path == "/zones/\($0.address)/state" } }),
              let id = zone.rooms.first(where: { path == "/zones/\($0.address)/state" }) else { throw ClientError.unreadable }
        if failing.contains(id) { throw ClientError.http(503, "Room unavailable") }
        let returned = wrongGroup.contains(id) ? recoveryZone("OTHER", [id.room]) : zone
        let state = ZoneState(zone: returned, transportState: "paused", volume: volume, mute: false,
                              groupVolume: volume, track: nil, shuffle: false, repeat: "off")
        return HTTPReply(data: try encoder.encode(state), status: 200)
    }
    nonisolated func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> {
        AsyncThrowingStream { $0.finish() }
    }
}

private func recoveryZone(_ home: String = "S2", _ rooms: [String] = ["A", "B"]) -> Zone {
    Zone(household: home, coordinatorRoom: rooms[0], members: rooms, transportState: "paused")
}
private func recoveryClient(_ fixture: RecoveryFixture) throws -> DaemonClient {
    DaemonClient(endpoint: try DaemonEndpoint("https://example.invalid"), transport: fixture)
}
private func aged(_ snapshot: HouseSnapshot) -> HouseSnapshot {
    HouseSnapshot(zones: snapshot.zones, states: snapshot.states, failures: snapshot.failures,
                  observedAt: Date(timeIntervalSince1970: 100))
}

@Test func houseRecoveryRetriesOnlyMissingRoomsAndPreservesHealthyPlaybackClocks() async throws {
    let fixture = RecoveryFixture([recoveryZone(), recoveryZone("S1", ["A"])])
    let client = try recoveryClient(fixture), failed = RoomID(household: "S2", room: "A")
    await fixture.configure(failing: [failed])
    let before = aged(try await client.houseSnapshot())
    await fixture.configure(volume: 33); await fixture.clearPaths()
    let after = try await client.recoverHouse(before)
    #expect(after.isComplete && after.states.count == 3)
    #expect(await fixture.paths == ["/zones", "/zones/A@S2/state"])
    #expect(after.states[failed]?.volume == 33)
    for id in before.states.keys {
        #expect(after.states[id] == before.states[id])
        #expect(after.stateObservedAt[id] == before.stateObservedAt[id])
    }
    #expect(try #require(after.stateObservedAt[failed]) > before.observedAt)
}

@Test func houseRecoveryReadsAllRegroupedAndNewRoomsButNotUnchangedHouseholds() async throws {
    let fixture = RecoveryFixture([recoveryZone(), recoveryZone("S1", ["A"])])
    let client = try recoveryClient(fixture)
    let before = aged(try await client.houseSnapshot())
    let changed = [recoveryZone("S2", ["B", "C"]), recoveryZone("S1", ["A"])]
    await fixture.configure(groups: changed, volume: 45); await fixture.clearPaths()
    let after = try await client.recoverHouse(before)
    #expect(after.zones == changed && after.isComplete)
    #expect(after.states[RoomID(household: "S2", room: "A")] == nil)
    #expect(Set(await fixture.paths) == ["/zones", "/zones/B@S2/state", "/zones/C@S2/state"])
    #expect(after.states[RoomID(household: "S1", room: "A")]?.volume == 20)
    #expect(after.stateObservedAt[RoomID(household: "S1", room: "A")] == before.observedAt)
    #expect(after.states[RoomID(household: "S2", room: "B")]?.volume == 45)
}

@Test func houseRecoveryDropsDepartedFailuresAndAcceptsAnObservedEmptyHouse() async throws {
    let fixture = RecoveryFixture([recoveryZone()]), failed = RoomID(household: "S2", room: "A")
    let client = try recoveryClient(fixture)
    await fixture.configure(failing: [failed])
    let before = try await client.houseSnapshot()
    await fixture.configure(groups: []); await fixture.clearPaths()
    let after = try await client.recoverHouse(before)
    #expect(after.isComplete && after.zones.isEmpty && after.states.isEmpty)
    #expect(after.stateObservedAt.isEmpty && after.failures.isEmpty)
    #expect(await fixture.paths == ["/zones"])
}

@Test func houseRecoveryTopologyFailureNeverFallsBackToCachedMembership() async throws {
    let fixture = RecoveryFixture([recoveryZone()])
    let client = try recoveryClient(fixture), before = try await client.houseSnapshot()
    await fixture.configure(topologyFails: true); await fixture.clearPaths()
    await #expect(throws: ClientError.self) { try await client.recoverHouse(before) }
    #expect(await fixture.paths == ["/zones"])
    await fixture.configure(groups: [recoveryZone(), recoveryZone()]); await fixture.clearPaths()
    await #expect(throws: ContractError.self) { try await client.recoverHouse(before) }
    #expect(await fixture.paths == ["/zones"])
    #expect(before.isComplete && before.states.count == 2)
}

@Test func houseRecoveryKeepsFailedStateAbsentAndHonorsExplicitHealthyInvalidation() async throws {
    let failed = RoomID(household: "S2", room: "A"), healthy = RoomID(household: "S2", room: "B")
    let fixture = RecoveryFixture([recoveryZone()]), client = try recoveryClient(fixture)
    await fixture.configure(failing: [failed])
    let before = try await client.houseSnapshot()
    await fixture.configure(wrongGroup: [failed], volume: 42); await fixture.clearPaths()
    let after = try await client.recoverHouse(before, refreshing: [healthy, RoomID(household: "ABSENT", room: "B")])
    #expect(after.states[failed] == nil && after.stateObservedAt[failed] == nil)
    #expect(after.failures[failed] != nil && !after.isComplete)
    #expect(after.states[healthy]?.volume == 42)
    #expect(Set(await fixture.paths) == ["/zones", "/zones/A@S2/state", "/zones/B@S2/state"])
}

@Test func houseRecoveryMemberReorderingDoesNotRereadHealthyGroups() async throws {
    let first = recoveryZone(), fixture = RecoveryFixture([recoveryZone()])
    let client = try recoveryClient(fixture), before = aged(try await client.houseSnapshot())
    let reordered = Zone(household: first.household, coordinatorRoom: first.coordinatorRoom,
                         members: first.members.reversed(), transportState: "playing")
    await fixture.configure(groups: [reordered]); await fixture.clearPaths()
    let after = try await client.recoverHouse(before)
    #expect(after.zones == [reordered] && after.states == before.states)
    #expect(after.stateObservedAt == before.stateObservedAt)
    #expect(await fixture.paths == ["/zones"])
}

@Test func houseRecoveryDoesNotRetainUnvalidatedOrUndatedObservations() async throws {
    let fixture = RecoveryFixture([recoveryZone()]), client = try recoveryClient(fixture)
    let before = try await client.houseSnapshot()
    let a = RoomID(household: "S2", room: "A"), b = RoomID(household: "S2", room: "B")
    var states = before.states
    states[a] = ZoneState(zone: recoveryZone("OTHER"), transportState: "paused", volume: 20,
                          mute: false, groupVolume: 20, track: nil, shuffle: nil, repeat: nil)
    let invalid = HouseSnapshot(zones: before.zones, states: states, failures: [:],
                                observedAt: before.observedAt, stateObservedAt: [a: before.observedAt])
    await fixture.configure(volume: 37); await fixture.clearPaths()
    let after = try await client.recoverHouse(invalid)
    #expect(after.isComplete && after.states[a]?.volume == 37 && after.states[b]?.volume == 37)
    #expect(await fixture.paths.count == 3)
}

@Test func houseRecoveryBoundsFanoutAndCancelsBeforeRemainingRooms() async throws {
    let fixture = RecoveryFixture([recoveryZone()]), client = try recoveryClient(fixture)
    let before = try await client.houseSnapshot()
    let larger = [recoveryZone("S2", (0..<12).map { "Room\($0)" })]
    await fixture.configure(groups: larger, delay: .seconds(30)); await fixture.clearPaths()
    let task = Task { try await client.recoverHouse(before) }
    for _ in 0..<1_000 {
        if await fixture.active == 4 { break }
        try await Task.sleep(for: .milliseconds(1))
    }
    #expect(await fixture.active == 4)
    task.cancel()
    await #expect(throws: CancellationError.self) { try await task.value }
    #expect(await fixture.paths.count == 5)
    #expect(await fixture.peak == 4)
    #expect(await fixture.active == 0)
    let tooMany = (0..<5).map { recoveryZone("House\($0)", (0..<256).map { "Room\($0)" }) }
    await fixture.configure(groups: tooMany); await fixture.clearPaths()
    await #expect(throws: ContractError.self) { try await client.recoverHouse(before) }
    #expect(await fixture.paths == ["/zones"])
}
