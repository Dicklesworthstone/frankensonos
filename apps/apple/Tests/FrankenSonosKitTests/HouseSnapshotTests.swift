import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

private actor SnapshotFixture: DaemonTransport {
    var groups: [Zone]
    var failed = Set<String>()
    var mismatched = Set<String>()
    var requests: [String] = []
    var active = 0, peak = 0, cancelled = 0
    var delay: Duration = .zero

    init(_ groups: [Zone]) { self.groups = groups }
    func configure(failed: Set<String> = [], mismatched: Set<String> = [], delay: Duration = .zero) {
        self.failed = failed; self.mismatched = mismatched; self.delay = delay
    }
    func send(_ request: URLRequest) async throws -> HTTPReply {
        #expect(request.httpMethod == nil || request.httpMethod == "GET")
        let path = request.url!.path
        requests.append(path)
        let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
        if path == "/zones" { return HTTPReply(data: try encoder.encode(groups), status: 200) }
        let address = String(path.dropFirst("/zones/".count).dropLast("/state".count))
        active += 1; peak = max(peak, active)
        defer { active -= 1 }
        do { try await Task.sleep(for: delay) } catch { cancelled += 1; throw error }
        if failed.contains(address) { throw ClientError.http(503, "Test speaker unavailable") }
        guard let zone = groups.first(where: { z in z.members.contains { "\($0)@\(z.household)" == address } }) else {
            throw ClientError.unreadable
        }
        let actual = mismatched.contains(address)
            ? Zone(household: "OTHER", coordinatorRoom: zone.coordinatorRoom, members: zone.members, transportState: "paused") : zone
        let value = ZoneState(zone: actual, transportState: "paused", volume: zone.household == "S1" ? 11 : 22,
                              mute: false, groupVolume: 22, track: nil, shuffle: false, repeat: "off")
        return HTTPReply(data: try encoder.encode(value), status: 200)
    }
    nonisolated func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> {
        AsyncThrowingStream { $0.finish() }
    }
}

private func snapshotZone(_ household: String, _ rooms: [String]) -> Zone {
    Zone(household: household, coordinatorRoom: rooms[0], members: rooms, transportState: "paused")
}
private func snapshotClient(_ fixture: SnapshotFixture) throws -> DaemonClient {
    DaemonClient(endpoint: try DaemonEndpoint("https://example.invalid"), transport: fixture)
}

@Test func snapshotReadsEveryMemberAndSeparatesSameNamedHouseholds() async throws {
    let groups = [snapshotZone("S1", ["Den", "Study"]), snapshotZone("S2", ["Den", "Kitchen"])]
    let fixture = SnapshotFixture(groups)
    let result = try await snapshotClient(fixture).houseSnapshot()
    #expect(result.zones == groups)
    #expect(result.states.count == 4 && result.isComplete)
    #expect(Set(result.stateObservedAt.keys) == Set(result.states.keys))
    #expect(result.stateObservedAt.values.allSatisfy { $0 <= result.observedAt })
    #expect(result.states[RoomID(household: "S1", room: "Den")]?.volume == 11)
    #expect(result.states[RoomID(household: "S2", room: "Den")]?.volume == 22)
    #expect(result.affected(by: ["Den"]).count == 4)
    #expect(result.affected(by: ["Kitchen"]) == groups[1].rooms)
    #expect(await fixture.requests.count == 5)
}

@Test func snapshotReportsAnOutageWithoutDiscardingHealthyRooms() async throws {
    let fixture = SnapshotFixture([snapshotZone("S2", ["A", "B", "C", "D", "E"])])
    await fixture.configure(failed: ["A@S2"])
    let result = try await snapshotClient(fixture).houseSnapshot()
    #expect(!result.isComplete && result.states.count == 4)
    #expect(result.failures.count == 1)
    #expect(result.failures[RoomID(household: "S2", room: "A")]?.contains("unavailable") == true)
    #expect(await fixture.requests.count == 6)
}

@Test func snapshotEmptyHouseIsNotAnError() async throws {
    let fixture = SnapshotFixture([])
    let result = try await snapshotClient(fixture).houseSnapshot()
    #expect(result.zones.isEmpty && result.states.isEmpty && result.isComplete)
    #expect(await fixture.requests == ["/zones"])
}

@Test func snapshotRejectsAmbiguousTopologyBeforeStateReads() async throws {
    let zone = snapshotZone("S2", ["Den"])
    let fixture = SnapshotFixture([zone, zone])
    await #expect(throws: ContractError.self) { try await snapshotClient(fixture).houseSnapshot() }
    #expect(await fixture.requests == ["/zones"])
}

@Test func snapshotRejectsWrongGroupStateWithoutPoisoningItsSiblings() async throws {
    let fixture = SnapshotFixture([snapshotZone("S2", ["A", "B"])])
    await fixture.configure(mismatched: ["A@S2"])
    let result = try await snapshotClient(fixture).houseSnapshot()
    #expect(result.states[RoomID(household: "S2", room: "A")] == nil)
    #expect(result.failures.count == 1 && result.states.count == 1)
}

@Test func snapshotFanOutIsBoundedAndStillVisitsEveryRoom() async throws {
    let fixture = SnapshotFixture([snapshotZone("S2", (0..<12).map { "Room\($0)" })])
    await fixture.configure(delay: .milliseconds(5))
    let result = try await snapshotClient(fixture).houseSnapshot()
    #expect(result.states.count == 12)
    #expect(await fixture.peak == 4)
    #expect(await fixture.requests.count == 13)
}

@Test func snapshotCancellationStopsTheRemainingRequests() async throws {
    let fixture = SnapshotFixture([snapshotZone("S2", (0..<12).map { "Room\($0)" })])
    await fixture.configure(delay: .seconds(30))
    let client = try snapshotClient(fixture)
    let task = Task { try await client.houseSnapshot() }
    for _ in 0..<1_000 {
        if await fixture.active == 4 { break }
        try await Task.sleep(for: .milliseconds(1))
    }
    #expect(await fixture.active == 4)
    task.cancel()
    await #expect(throws: CancellationError.self) { try await task.value }
    #expect(await fixture.requests.count == 5)
    #expect(await fixture.cancelled == 4)
}

@Test func snapshotTargetedRefreshDropsStaleFailedValuesAndPreservesOtherRooms() async throws {
    let fixture = SnapshotFixture([snapshotZone("S1", ["Den"]), snapshotZone("S2", ["Den"])])
    let client = try snapshotClient(fixture)
    let before = try await client.houseSnapshot()
    let id = RoomID(household: "S1", room: "Den")
    await fixture.configure(failed: [id.address])
    let failed = try await client.refresh(before, rooms: [id])
    #expect(failed.states[id] == nil && failed.failures[id] != nil)
    #expect(failed.stateObservedAt[id] == nil)
    #expect(failed.states[RoomID(household: "S2", room: "Den")] == before.states[RoomID(household: "S2", room: "Den")])
    await fixture.configure()
    let recovered = try await client.refresh(failed, rooms: [id])
    #expect(recovered.isComplete && recovered.states[id] != nil)
    #expect(recovered.stateObservedAt[id] != nil)
    #expect(await fixture.requests.count == 5)
}

@Test func snapshotTargetedRefreshPreservesOtherRoomsPlaybackClocks() async throws {
    let fixture = SnapshotFixture([snapshotZone("S1", ["Den"]), snapshotZone("S2", ["Den"])])
    let client = try snapshotClient(fixture)
    let observation = try await client.houseSnapshot()
    let initialDate = Date(timeIntervalSince1970: 100)
    let before = HouseSnapshot(zones: observation.zones, states: observation.states, failures: [:], observedAt: initialDate)
    let changed = RoomID(household: "S1", room: "Den"), unchanged = RoomID(household: "S2", room: "Den")
    let after = try await client.refresh(before, rooms: [changed])
    #expect(after.stateObservedAt[unchanged] == initialDate)
    #expect(try #require(after.stateObservedAt[changed]) > initialDate)
    #expect(after.stateObservedAt[changed] != after.stateObservedAt[unchanged])
    #expect(await fixture.requests.count == 4)
}

@Test func snapshotBoundsTheTotalHouseBeforeDispatching() async throws {
    let groups = (0..<5).map { household in
        snapshotZone("House\(household)", (0..<256).map { "Room\($0)" })
    }
    let fixture = SnapshotFixture(groups)
    await #expect(throws: ContractError.self) { try await snapshotClient(fixture).houseSnapshot() }
    #expect(await fixture.requests == ["/zones"])
}
