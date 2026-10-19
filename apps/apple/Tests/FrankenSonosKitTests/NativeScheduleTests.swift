import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

let scheduleRoom = RoomID(household: "TEST-A", room: "Studio")
let scheduleOtherRoom = RoomID(household: "TEST-B", room: "Studio")
let scheduleWeekdays = ScheduleTime.weekly(days: [.mon, .tue, .wed, .thu, .fri], at: "07:30")
func scheduleZone(_ household: String = "TEST-A") -> Zone {
    Zone(household: household, coordinatorRoom: "Studio", members: ["Studio", "Hall"], transportState: "playing")
}
func scheduleTestScene(_ name: String = "Evening / Quiet & rest") -> Scene {
    Scene(name: name, saved: 1, groups: [], volumes: ["Studio@TEST-A": 20, "Studio@TEST-B": 30], mutes: [:])
}
func scheduleFixture(_ patch: [String: JSONValue] = [:], removing: [String] = []) -> [String: JSONValue] {
    var fields: [String: JSONValue] = ["id": .integer(7), "when": .string("weekdays 07:30"), "action": .string("dj_start"),
        "zone": .string("Studio@TEST-A"), "mood": .string("bright"), "summary": .string("Start the DJ in Studio"),
        "creator": .string("loopback-http"), "enabled": .bool(true), "next": .string("2028-03-13T07:30:00-04:00")]
    fields.merge(patch) { _, new in new }
    removing.forEach { fields.removeValue(forKey: $0) }
    return fields
}
func scheduleDecode(_ row: [String: JSONValue]) throws -> SavedSchedule {
    try DaemonClient.decode(SavedSchedule.self, from: JSONEncoder().encode(row))
}

/// Production client codecs and shared command actor with a controlled daemon.
/// These fixtures deliberately implement storage only, never schedule execution.
actor ScheduleTestTransport: DaemonTransport {
    var requests: [URLRequest] = []
    var rows: [[String: JSONValue]] = [scheduleFixture()]
    var topology = [scheduleZone(), scheduleZone("TEST-B")]
    var savedScene = scheduleTestScene()
    var failedRead: String?
    var wrongScene = false
    var loseReply = false
    var replyOverride: HTTPReply?
    private var nextID: Int64 = 20
    private var blockKey: String?
    private var gate: CheckedContinuation<Void, Never>?
    var blocked = false

    func setRows(_ value: [[String: JSONValue]]) { rows = value }
    func setTopology(_ value: [Zone]) { topology = value }
    func setScene(_ value: Scene) { savedScene = value }
    func setReadFailure(_ value: String?) { failedRead = value }
    func loseNextReply() { loseReply = true }
    func overrideReply(_ value: HTTPReply?) { replyOverride = value }
    func blockNext(_ method: String, _ path: String) { blockKey = method + " " + path }
    func release() { let saved = gate; gate = nil; blocked = false; saved?.resume() }
    func writes() -> [URLRequest] { requests.filter { $0.httpMethod == "POST" } }
    func send(_ request: URLRequest) async throws -> HTTPReply {
        requests.append(request)
        let path = request.url!.path.removingPercentEncoding ?? request.url!.path
        let method = request.httpMethod ?? "GET"
        if blockKey == method + " " + path {
            blockKey = nil; blocked = true
            // Keep a cancelled read pending until explicitly released.
            await withCheckedContinuation { gate = $0 }
        }
        if method == "GET" {
            if failedRead == path { throw URLError(.cannotConnectToHost) }
            if path == "/schedules" { return try json(rows) }
            if path == "/zones" { return try json(topology, snake: true) }
            if path.hasSuffix("/state") {
                let id = path.split(separator: "/")[1].description
                guard let zone = topology.first(where: { $0.rooms.contains { $0.address == id } }) else {
                    throw URLError(.cannotFindHost)
                }
                return try json(ZoneState(zone: zone, transportState: "playing", volume: 20, mute: false,
                    groupVolume: 20, track: nil, shuffle: false, repeat: "off"), snake: true)
            }
            if path.hasPrefix("/scenes/") { return try json(savedScene, snake: true) }
            if path == "/scenes" { return try json([savedScene], snake: true) }
            throw ContractError.invalid("Unexpected read: " + path)
        }
        let body = try JSONDecoder().decode([String: JSONValue].self, from: request.httpBody!)
        if let replyOverride { return replyOverride }
        let response: HTTPReply
        if path == "/schedules" {
            var row = body
            row["id"] = .integer(nextID); nextID += 1
            row["summary"] = .string("Stored action")
            row["creator"] = .string("loopback-http")
            row["enabled"] = .bool(true)
            if case .string(let when) = row["when"], when.hasPrefix("once ") {
                row["next"] = .string(String(when.dropFirst(5)))
            } else { row["next"] = .string("2028-03-13T07:30:00-04:00") }
            rows.append(row); response = try json(row)
        } else if path.hasPrefix("/schedules/") {
            guard let index = rows.firstIndex(where: { $0["id"] == body["id"] }) else {
                return try json(["code": "UNKNOWN_SCHEDULE", "detail": "Missing schedule"], status: 404)
            }
            var row = rows[index]
            switch path {
            case "/schedules/pause": row["enabled"] = .bool(false); row.removeValue(forKey: "next"); rows[index] = row
            case "/schedules/resume":
                row["enabled"] = .bool(true); row["last_fired"] = .string("2028-03-12T17:00:00Z")
                row["next"] = .string("2028-03-13T07:30:00-04:00"); rows[index] = row
            case "/schedules/remove": rows.remove(at: index)
            default: throw ContractError.invalid("Unexpected schedule mutation")
            }
            response = try json(row)
        } else if path == "/pause" { response = try json(["done": JSONValue.string("Paused"), "changed": .bool(true)]) }
        else { throw ContractError.invalid("Unexpected mutation: " + path) }
        if loseReply { loseReply = false; throw URLError(.networkConnectionLost) }
        return response
    }
    nonisolated func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> {
        AsyncThrowingStream { $0.finish() }
    }
    private func json<T: Encodable>(_ value: T, status: Int = 200, snake: Bool = false) throws -> HTTPReply {
        let encoder = JSONEncoder()
        if snake { encoder.keyEncodingStrategy = .convertToSnakeCase }
        return HTTPReply(data: try encoder.encode(value), status: status)
    }
}

func scheduleClient(_ transport: ScheduleTestTransport) throws -> DaemonClient {
    DaemonClient(endpoint: try DaemonEndpoint("https://music.example.test"), transport: transport)
}
func scheduleBody(_ request: URLRequest) throws -> [String: JSONValue] {
    try JSONDecoder().decode([String: JSONValue].self, from: request.httpBody!)
}
func waitForScheduleGate(_ transport: ScheduleTestTransport) async throws {
    let deadline = ContinuousClock.now.advanced(by: .seconds(2))
    while !(await transport.blocked) {
        guard ContinuousClock.now < deadline else { throw ContractError.invalid("Schedule test gate not reached") }
        try await Task.sleep(for: .milliseconds(2))
    }
}

@Test func nativeScheduleTimesAreHouseLocalCanonicalAndOffsetExplicit() throws {
    #expect(try scheduleWeekdays.canonical() == "weekdays 07:30")
    #expect(try ScheduleTime.weekly(days: Set(ScheduleDay.allCases), at: "00:00").canonical() == "daily 00:00")
    #expect(try ScheduleTime.weekly(days: [.sun, .sat], at: "23:59").canonical() == "weekends 23:59")
    #expect(try ScheduleTime.weekly(days: [.sun, .mon], at: "07:30").canonical() == "mon,sun 07:30")
    let one = ScheduleTime.once(at: "2028-03-12T07:30:00-04:00")
    #expect(try one.canonical() == "once 2028-03-12T07:30:00-04:00")
    #expect(one.sameTime(as: .once(at: "2028-03-12T11:30:00Z")))
    #expect(!one.sameTime(as: .once(at: "2028-03-12T07:30:00Z")))
    #expect(try ScheduleTime.stored("mon,tue,wed,thu,fri 07:30").sameTime(as: scheduleWeekdays))
}

@Test func nativeScheduleRejectsInvalidDaysTimesDatesAndCoercions() throws {
    for text in ["", "7:30", "24:00", "07:60", "07:30:00", " 07:30", "０７:３０", "07:30\n"] {
        #expect(throws: ContractError.self) { try ScheduleTime.weekly(days: [.mon], at: text).canonical() }
    }
    #expect(throws: ContractError.self) { try ScheduleTime.weekly(days: [], at: "07:30").canonical() }
    for text in ["2028-02-30T07:30:00Z", "2028-13-01T07:30:00Z", "2028-02-01T25:30:00Z",
                 "2028-03-12T07:30:00+24:00", "2028-03-12T07:30:00-04:60", "2028-03-12T07:30:00", "2028-03-12", "2028-03-12T07:30:00Zjunk", "2028-03-12T07:30:00.9Z", "2028-03-12T07:30:00Z\n", "2028-03-12T07:30:00Z\r"] {
        #expect(throws: ContractError.self) { try ScheduleTime.once(at: text).canonical() }
    }
}

@Test func nativeSchedulePayloadsRepresentAllFourActionsWithoutInventingCreator() throws {
    let actions: [(ScheduledAction, [String: JSONValue])] = [
        (.djStart(room: scheduleOtherRoom, mood: " Bright "), ["action": .string("dj_start"), "zone": .string("Studio@TEST-B"), "mood": .string("bright")]),
        (.pause(room: scheduleRoom, fadeSecs: 600), ["action": .string("pause"), "zone": .string("Studio@TEST-A"), "fade_secs": .integer(600)]),
        (.volume(room: RoomID(household: "TEST-B", room: "Hall"), level: 35), ["action": .string("volume"), "zone": .string("Hall@TEST-B"), "volume": .integer(35)]),
        (.scene(scheduleTestScene()), ["action": .string("apply_scene"), "scene": .string("Evening / Quiet & rest")])]
    for (action, fields) in actions {
        let payload = try ScheduleCommand.add(ScheduleDraft(time: scheduleWeekdays, action: action)).payload()
        #expect(payload.path == ["schedules"])
        #expect(payload.body == fields.merging(["when": .string("weekdays 07:30")]) { _, new in new })
        #expect(payload.body["creator"] == nil)
    }
    #expect(try ScheduledAction.pause(room: scheduleRoom, fadeSecs: 0).fields()["fade_secs"] == nil)
    #expect(throws: ContractError.self) { try ScheduledAction.pause(room: scheduleRoom, fadeSecs: 601).fields() }
    #expect(throws: ContractError.self) { try ScheduledAction.volume(room: scheduleRoom, level: -1).fields() }
    #expect(throws: ContractError.self) { try ScheduledAction.volume(room: scheduleRoom, level: 101).fields() }
}

@Test func nativeScheduleListPreservesLargeIDsAndRejectsAmbiguousReplies() async throws {
    let transport = ScheduleTestTransport(), client = try scheduleClient(transport)
    let large: Int64 = 9_007_199_254_740_993
    await transport.setRows([scheduleFixture(["id": .integer(large)])])
    let saved = try #require(try await client.schedules().first)
    #expect(saved.id == large)
    #expect(try ScheduleCommand.change(saved, .pause).payload().body["id"] == .integer(large))
    await transport.setRows([scheduleFixture(), scheduleFixture()])
    await #expect(throws: ContractError.self) { try await client.schedules() }
    await transport.setRows([scheduleFixture(["enabled": .bool(false)])])
    await #expect(throws: ContractError.self) { try await client.schedules() }
    await transport.setRows([])
    #expect(try await client.schedules().isEmpty)
}

@Test func nativeScheduleMalformedActionsAndStateNeverBecomeActionable() throws {
    let corrupt: [([String: JSONValue], [String])] = [
        (["id": .integer(0)], []), (["action": .string("unknown")], []), (["zone": .string("")], []),
        (["scene": .string("Unexpected")], []), (["next": .string("tomorrow")], []),
        (["last_fired": .string("now")], []), (["creator": .string("")], []),
        (["when": .string("mon 25:00")], []), (["when": .string("1 07:30")], []),
        (["action": .string("volume"), "volume": .integer(101)], ["mood"]),
        (["action": .string("pause"), "fade_secs": .integer(601)], ["mood"])]
    for (fields, removed) in corrupt {
        #expect(throws: (any Error).self) { try scheduleDecode(scheduleFixture(fields, removing: removed)).validated() }
    }
    #expect(try scheduleDecode(scheduleFixture(["enabled": .bool(false)], removing: ["next"])).validated().stateLabel == "Paused")
}

@Test func nativeScheduleCreateReadsExactTargetThenStoresOnceForEachAction() async throws {
    let actions: [ScheduledAction] = [.djStart(room: scheduleOtherRoom, mood: "bright"), .pause(room: scheduleRoom, fadeSecs: 90),
        .volume(room: RoomID(household: "TEST-B", room: "Hall"), level: 25), .scene(scheduleTestScene())]
    for action in actions {
        let transport = ScheduleTestTransport(), session = try CommandReviewSession(client: scheduleClient(transport))
        let review = try await session.prepare(ScheduleCommand.add(ScheduleDraft(time: scheduleWeekdays, action: action)))
        #expect(await transport.writes().isEmpty)
        let receipt = try await session.execute(review)
        #expect(receipt.schedule.id == 20 && !receipt.removed)
        #expect(receipt.detail.contains("not successful playback"))
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes().count == 1)
        let request = try #require(await transport.writes().first)
        #expect(request.url?.path == "/schedules")
        #expect(request.value(forHTTPHeaderField: "Authorization") == nil)
        #expect(request.value(forHTTPHeaderField: "Content-Type") == "application/json")
        #expect(try scheduleBody(request)["creator"] == nil)
        #expect(await transport.requests.filter { $0.url?.path.hasSuffix("/state") == true }.isEmpty)
    }
}

@Test func nativeScheduleCreationKeepsOneTimeOffsetAndDoesNotUseMacTimezone() async throws {
    let transport = ScheduleTestTransport(), session = try CommandReviewSession(client: scheduleClient(transport))
    let time = ScheduleTime.once(at: "2028-03-12T07:30:00-04:00")
    let review = try await session.prepare(ScheduleCommand.add(ScheduleDraft(time: time, action: .pause(room: scheduleRoom, fadeSecs: 0))))
    let receipt = try await session.execute(review)
    #expect(receipt.schedule.when == "once 2028-03-12T07:30:00-04:00")
    #expect(receipt.schedule.next == "2028-03-12T07:30:00-04:00")
    #expect(try scheduleBody(#require(await transport.writes().first))["when"] == .string("once 2028-03-12T07:30:00-04:00"))
}

@Test func nativeScheduleChangedOrMissingTargetCannotCrossHouseholds() async throws {
    let transport = ScheduleTestTransport(), session = try CommandReviewSession(client: scheduleClient(transport))
    let review = try await session.prepare(ScheduleCommand.add(ScheduleDraft(time: scheduleWeekdays, action: .pause(room: scheduleOtherRoom, fadeSecs: 0))))
    await transport.setTopology([scheduleZone()])
    await #expect(throws: ContractError.self) { try await session.execute(review) }
    #expect(await transport.writes().isEmpty)
}

@Test func nativeScheduleSceneCreationRejectsAReplacedDefinitionBeforeStoreWrite() async throws {
    let transport = ScheduleTestTransport(), session = try CommandReviewSession(client: scheduleClient(transport))
    let scene = scheduleTestScene()
    let review = try await session.prepare(ScheduleCommand.add(ScheduleDraft(time: scheduleWeekdays, action: .scene(scene))))
    await transport.setScene(Scene(name: scene.name, saved: 2, groups: [], volumes: ["Studio@TEST-B": 60], mutes: [:]))
    await #expect(throws: ContractError.self) { try await session.execute(review) }
    #expect(await transport.writes().isEmpty)
    #expect(await transport.requests.filter { $0.url?.path == "/zones" }.isEmpty)
}

@Test func nativeSchedulePauseResumeAndRemoveDoNotRequireRoomsOrPlayback() async throws {
    let transport = ScheduleTestTransport(), client = try scheduleClient(transport)
    let session = CommandReviewSession(client: client)
    await transport.setTopology([]); await transport.setReadFailure("/zones")
    for change in [ScheduleChange.pause, .resume, .remove] {
        let schedule = try #require(try await client.schedules().first)
        let review = try await session.prepare(ScheduleCommand.change(schedule, change))
        let result = try await session.execute(review)
        #expect(result.schedule.id == schedule.id)
        #expect(result.removed == (change == .remove))
        if change == .pause { #expect(!result.schedule.enabled && result.schedule.next == nil) }
        if change == .resume { #expect(result.schedule.enabled && result.schedule.lastFired != nil) }
    }
    #expect(await transport.writes().map { $0.url!.path } == ["/schedules/pause", "/schedules/resume", "/schedules/remove"])
    #expect(await transport.requests.allSatisfy { $0.url!.path.hasPrefix("/schedules") })
    #expect(await transport.rows.isEmpty)
}

@Test func nativeScheduleProgressDoesNotInvalidateButDefinitionChangesDo() async throws {
    let transport = ScheduleTestTransport(), client = try scheduleClient(transport), session = CommandReviewSession(client: client)
    let saved = try #require(try await client.schedules().first)
    let review = try await session.prepare(ScheduleCommand.change(saved, .pause))
    await transport.setRows([scheduleFixture(["next": .string("2028-03-14T07:30:00-04:00"), "last_fired": .string("2028-03-13T07:30:00-04:00"), "summary": .string("New prose")])])
    _ = try await session.execute(review)
    #expect(await transport.writes().count == 1)
    for replacement in [scheduleFixture(["when": .string("weekdays 08:00")]), scheduleFixture(["creator": .string("different-owner")]),
                        scheduleFixture(["zone": .string("Studio@TEST-B")]), scheduleFixture(["enabled": .bool(false)], removing: ["next"])] {
        await transport.setRows([scheduleFixture()])
        let pending = try await session.prepare(ScheduleCommand.change(saved, .remove))
        await transport.setRows([replacement])
        await #expect(throws: ContractError.self) { try await session.execute(pending) }
    }
    #expect(await transport.writes().count == 1)
}

@Test func nativeScheduleDeletedDuringReviewCannotOperateOnAnotherID() async throws {
    let transport = ScheduleTestTransport(), client = try scheduleClient(transport), session = CommandReviewSession(client: client)
    let review = try await session.prepare(ScheduleCommand.change(#require(try await client.schedules().first), .remove))
    await transport.setRows([scheduleFixture(["id": .integer(8)])])
    await #expect(throws: ContractError.self) { try await session.execute(review) }
    #expect(await transport.writes().isEmpty)
}

@Test func nativeScheduleFailedReadIsNotAnEmptyListOrPermissionToMutate() async throws {
    let transport = ScheduleTestTransport(), client = try scheduleClient(transport), session = CommandReviewSession(client: client)
    let saved = try #require(try await client.schedules().first)
    let review = try await session.prepare(ScheduleCommand.change(saved, .remove))
    await transport.setReadFailure("/schedules")
    await #expect(throws: URLError.self) { try await client.schedules() }
    await #expect(throws: URLError.self) { try await session.execute(review) }
    #expect(await transport.writes().isEmpty)
}

@Test func nativeScheduleLostCreationReceiptIsNeverReplayedOrSilentlyRemoved() async throws {
    let transport = ScheduleTestTransport(), session = try CommandReviewSession(client: scheduleClient(transport))
    let review = try await session.prepare(ScheduleCommand.add(ScheduleDraft(time: scheduleWeekdays, action: .volume(room: scheduleRoom, level: 25))))
    await transport.loseNextReply()
    await #expect(throws: ClientError.self) { try await session.execute(review) }
    await #expect(throws: ContractError.self) { try await session.execute(review) }
    await session.cancel()
    #expect(await transport.rows.count == 2) // the write really took effect in this fixture
    #expect(await transport.writes().count == 1)
}

@Test func nativeScheduleContradictoryAcknowledgementsStayUncertainWithoutRetry() async throws {
    let replies = [scheduleFixture(["id": .integer(9), "enabled": .bool(false)], removing: ["next"]),
                   scheduleFixture(["zone": .string("Studio@TEST-B"), "enabled": .bool(false)], removing: ["next"]),
                   scheduleFixture()]
    for reply in replies {
        let transport = ScheduleTestTransport(), client = try scheduleClient(transport), session = CommandReviewSession(client: client)
        let review = try await session.prepare(ScheduleCommand.change(#require(try await client.schedules().first), .pause))
        await transport.overrideReply(HTTPReply(data: try JSONEncoder().encode(reply), status: 200))
        await #expect(throws: ClientError.self) { try await session.execute(review) }
        #expect(await transport.writes().count == 1)
    }
}

@Test func nativeScheduleWrongCreationActionOrTimeCannotBeAcknowledgedAsSuccess() throws {
    let command = ScheduleCommand.add(ScheduleDraft(time: scheduleWeekdays, action: .djStart(room: scheduleRoom, mood: "bright")))
    for row in [scheduleFixture(["when": .string("weekends 07:30")]), scheduleFixture(["zone": .string("Studio@TEST-B")]),
                scheduleFixture(["mood": .string("calm")]), scheduleFixture(["enabled": .bool(false)], removing: ["next"])] {
        #expect(throws: ClientError.self) { try command.receipt(JSONEncoder().encode(row)) }
    }
}

@Test func nativeScheduleCancellationAfterAHeldPreflightPreventsDispatch() async throws {
    let transport = ScheduleTestTransport(), client = try scheduleClient(transport), session = CommandReviewSession(client: client)
    let review = try await session.prepare(ScheduleCommand.change(#require(try await client.schedules().first), .pause))
    await transport.blockNext("GET", "/schedules")
    let task = Task { try await session.execute(review) }
    try await waitForScheduleGate(transport)
    await session.cancel()
    await #expect(throws: ContractError.self) { try await session.prepare(.pause, in: scheduleZone()) }
    await transport.release()
    await #expect(throws: CancellationError.self) { try await task.value }
    #expect(await transport.writes().isEmpty)
}

@Test func nativeScheduleAndPlaybackShareOneReviewAndOneBusySlot() async throws {
    let transport = ScheduleTestTransport(), client = try scheduleClient(transport), session = CommandReviewSession(client: client)
    let native = try await session.prepare(.pause, in: scheduleZone())
    let stored = try #require(try await client.schedules().first)
    let schedule = try await session.prepare(ScheduleCommand.change(stored, .pause))
    await #expect(throws: ContractError.self) { try await session.execute(native) }
    let replacement = try await session.prepare(.pause, in: scheduleZone())
    await #expect(throws: ContractError.self) { try await session.execute(schedule) }
    let review = try await session.prepare(ScheduleCommand.change(stored, .pause))
    await #expect(throws: ContractError.self) { try await session.execute(replacement) }
    await transport.blockNext("POST", "/schedules/pause")
    let task = Task { try await session.execute(review) }
    try await waitForScheduleGate(transport)
    await #expect(throws: ContractError.self) { try await session.prepare(.pause, in: scheduleZone()) }
    await #expect(throws: ContractError.self) { try await session.execute(review) }
    await session.cancel(); await transport.release()
    #expect(try await task.value.title == "Paused schedule #7")
    #expect(await transport.writes().count == 1)
}

@Test func nativeScheduleForeignAndCancelledReviewsCannotExecute() async throws {
    let transport = ScheduleTestTransport(), client = try scheduleClient(transport)
    let session = CommandReviewSession(client: client), other = CommandReviewSession(client: client)
    let review = try await session.prepare(ScheduleCommand.change(#require(try await client.schedules().first), .remove))
    await #expect(throws: ContractError.self) { try await other.execute(review) }
    await session.cancel()
    await #expect(throws: ContractError.self) { try await session.execute(review) }
    #expect(await transport.writes().isEmpty)
}

@Test func nativeScheduleAPIDenialsAndMalformedWriteRepliesRemainSingleShot() async throws {
    let denial = HTTPReply(data: Data(#"{"code":"POLICY_DENIED","detail":"Schedule denied","hint":"Check creator rights"}"#.utf8), status: 403)
    for reply in [denial, HTTPReply(data: Data("invalid".utf8), status: 200), HTTPReply(data: Data(), status: 307)] {
        let transport = ScheduleTestTransport(), session = try CommandReviewSession(client: scheduleClient(transport))
        let review = try await session.prepare(ScheduleCommand.add(ScheduleDraft(time: scheduleWeekdays, action: .pause(room: scheduleRoom, fadeSecs: 0))))
        await transport.overrideReply(reply)
        await #expect(throws: ClientError.self) { try await session.execute(review) }
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes().count == 1)
    }
}

@Test func nativeScheduleDateValidationIsTotalForMalformedNetworkText() {
    let original = Array("2028-03-12T07:30:00-04:00".utf8)
    for index in original.indices {
        for replacement: UInt8 in 0...127 {
            var bytes = original; bytes[index] = replacement
            _ = ScheduleTime.instant(String(decoding: bytes, as: UTF8.self))
        }
    }
    for suffix in ["\n", "\r", "\r\n", "\0", " ", "Z", "\u{2028}"] {
        #expect(ScheduleTime.instant("2028-03-12T07:30:00Z" + suffix) == nil)
    }
}
