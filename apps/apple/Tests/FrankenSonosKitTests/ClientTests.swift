import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

let zoneJSON = #"{"household":"S2","coordinator_room":"Studio","members":["Studio","Hall"],"transport_state":"playing"}"#
let stateJSON = #"{"zone":ZONE,"transport_state":"playing","volume":20,"group_volume":24,"mute":false,"track":{"uri":"test:work:1","title":"Example movement","duration_secs":240,"position_secs":32,"queue_position":2}}"#.replacingOccurrences(of: "ZONE", with: zoneJSON)
func fixture<T: Decodable>(_ type: T.Type, _ json: String) throws -> T { try DaemonClient.decode(type, from: Data(json.utf8)) }
func testZone() throws -> Zone { try fixture(Zone.self, zoneJSON) }
func testState() throws -> ZoneState { try fixture(ZoneState.self, stateJSON) }

actor RecordingTransport: DaemonTransport {
    var requests: [URLRequest] = []
    var replies: [HTTPReply]
    var failure: Bool
    init(_ json: String = "{}", status: Int = 200, failure: Bool = false) {
        replies = [HTTPReply(data: Data(json.utf8), status: status)]; self.failure = failure
    }
    func send(_ request: URLRequest) async throws -> HTTPReply {
        requests.append(request)
        if failure { throw URLError(.networkConnectionLost) }
        return replies.count > 1 ? replies.removeFirst() : replies[0]
    }
    nonisolated func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> {
        AsyncThrowingStream { $0.finish() }
    }
}
func client(_ transport: RecordingTransport) throws -> DaemonClient {
    DaemonClient(endpoint: try DaemonEndpoint("https://music.example.test"), transport: transport)
}

@Test func endpointRequiresExplicitLocalHTTP() throws {
    #expect(throws: ContractError.self) { try DaemonEndpoint("http://localhost:8787") }
    for address in ["http://localhost:8787", "http://[::1]:8787", "http://example.local:8787", "http://192.168.20.7:8787", "http://10.1.2.3:8787"] {
        #expect(try DaemonEndpoint(address, allowLocalHTTP: true).url.scheme == "http")
    }
    for address in ["http://music.example.test", "http://169.254.169.254", "http://2130706433", "ftp://example.local", "https://user:pass@music.example.test", "https://music.example.test/api", "https://music.example.test?q=x", "https://music.example.test/#x", "https://music.example.test:0"] {
        #expect(throws: ContractError.self) { try DaemonEndpoint(address, allowLocalHTTP: true) }
    }
}

@Test func pathSegmentsCannotChangeOriginOrRoute() throws {
    let endpoint = try DaemonEndpoint("https://music.example.test/")
    let url = try endpoint.resource(["zones", "Room / &?#%@S2", "state"], query: [URLQueryItem(name: "q", value: "Bach & strings")])
    #expect(url.host == "music.example.test")
    #expect(url.absoluteString.contains("%2F"))
    #expect(url.absoluteString.contains("%40S2/state"))
    #expect(URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems?.first?.value == "Bach & strings")
    #expect(throws: ContractError.self) { try endpoint.resource(["..", "elsewhere"]) }
}

@Test func householdIdentityAndGroupingAreNotDisplayStrings() throws {
    let zone = try testZone()
    var same = zoneJSON.replacingOccurrences(of: #"["Studio","Hall"]"#, with: #"["Hall","Studio"]"#)
    #expect(try zone.sameGroup(as: fixture(Zone.self, same)))
    same = zoneJSON.replacingOccurrences(of: "S2", with: "S1")
    #expect(try !zone.sameGroup(as: fixture(Zone.self, same)))
    #expect(throws: ContractError.self) { try Zone.validate([zone, zone]) }
    #expect(try Zone.validate([zone, fixture(Zone.self, same)]).count == 2)
    for json in [zoneJSON.replacingOccurrences(of: "Hall", with: "Studio"), zoneJSON.replacingOccurrences(of: #""members":["Studio","Hall"]"#, with: #""members":[]"#)] {
        #expect(throws: ContractError.self) { try fixture(Zone.self, json).validated() }
    }
}

@Test func malformedStateDoesNotEnableAControl() throws {
    let zone = try testZone()
    for json in [stateJSON.replacingOccurrences(of: #""volume":20"#, with: #""volume":101"#),
                 stateJSON.replacingOccurrences(of: #""position_secs":32"#, with: #""position_secs":300"#),
                 stateJSON.replacingOccurrences(of: "S2", with: "S1"),
                 stateJSON.replacingOccurrences(of: #""queue_position":2"#, with: #""queue_position":0"#)] {
        #expect(throws: ContractError.self) { try fixture(ZoneState.self, json).validated(for: zone) }
    }
    #expect(try testState().validated(for: zone).track?.positionSecs == 32)
}

@Test func exactVolumeAndQueuePayloads() throws {
    let zone = try testZone()
    let group = try NativeCommand.groupVolume(32).payload(in: zone)
    #expect(group.path == ["volume"])
    #expect(group.body == ["zone": .string("Studio@S2"), "volume": .integer(32), "group": .bool(true)])
    let room = try NativeCommand.volume(12, room: "Hall").payload(in: zone)
    #expect(room.body["zone"] == .string("Hall@S2"))
    #expect(room.body["group"] == nil)
    #expect(throws: ContractError.self) { try NativeCommand.volume(12, room: "Absent").payload(in: zone) }
    #expect(throws: ContractError.self) { try NativeCommand.groupVolume(101).payload(in: zone) }
    let item = try fixture(QueueItem.self, #"{"position":2,"title":"Second","uri":"test:exact?x=1&x=2"}"#)
    let play = try NativeCommand.queuePlay(item, revision: "opaque/revision").payload(in: zone)
    #expect(play.body["expected_uri"] == .string("test:exact?x=1&x=2"))
    #expect(play.body["expected_revision"] == .string("opaque/revision"))
    #expect(play.path == ["queue", "play"])
    let remove = try NativeCommand.queueRemove(item, revision: "r1").payload(in: zone)
    #expect(remove.body["expected_uris"] == .array([.string("test:exact?x=1&x=2")]))
    #expect(throws: ContractError.self) { try NativeCommand.queuePlay(item, revision: "").payload(in: zone) }
}

@Test func queueAddNeverBecomesAPlayCommand() async throws {
    let transport = RecordingTransport(#"{"done":"Queued","first":3,"count":4,"playing_queue":false}"#)
    let receipt = try await client(transport).perform(.queueAdd("spotify:album:EXAMPLE", next: true), in: testZone())
    let requests = await transport.requests
    #expect(requests.count == 1)
    #expect(requests[0].url?.path == "/queue")
    #expect(requests[0].httpMethod == "POST")
    #expect(try JSONDecoder().decode([String: JSONValue].self, from: requests[0].httpBody!) == [
        "zone": .string("Studio@S2"), "source_uri": .string("spotify:album:EXAMPLE"), "next": .bool(true)
    ])
    #expect(receipt.detail.contains("4 entries"))
}

@Test func policyAcknowledgementIsAuthoritative() async throws {
    let transport = RecordingTransport(#"{"done":"Group level 25","changed":true,"volume":25,"notes":[{"code":"VOLUME_CLAMPED","detail":"Capped at 25"}]}"#)
    let receipt = try await client(transport).perform(.groupVolume(80), in: testZone())
    #expect(receipt.detail.contains("Capped at 25"))
    #expect(await transport.requests.count == 1)
}

@Test func lostMalformedAndServerFailureWritesAreNeverRepeated() async throws {
    for transport in [RecordingTransport(failure: true), RecordingTransport("not JSON"), RecordingTransport("{}"),
                      RecordingTransport(#"{"code":"UNAVAILABLE","detail":"Offline"}"#, status: 503), RecordingTransport("", status: 307)] {
        await #expect(throws: ClientError.self) { try await client(transport).perform(.next, in: testZone()) }
        #expect(await transport.requests.count == 1)
    }
}

@Test func readFailuresPreserveBackendCodeAndHint() async throws {
    let transport = RecordingTransport(#"{"code":"POLICY_DENIED","detail":"Read denied","hint":"Check the daemon policy"}"#, status: 403)
    do { _ = try await client(transport).zones(); Issue.record("Expected failure") }
    catch let problem as APIProblem { #expect(problem.code == "POLICY_DENIED"); #expect(problem.hint == "Check the daemon policy") }
    #expect(await transport.requests.count == 1)
}

@Test func requestsDoNotForgeIdentityOrCarryCredentials() async throws {
    let transport = RecordingTransport("[\(zoneJSON)]")
    _ = try await client(transport).zones()
    let request = try #require(await transport.requests.first)
    #expect(request.value(forHTTPHeaderField: "Authorization") == nil)
    #expect(request.value(forHTTPHeaderField: "Tailscale-User-Login") == nil)
    #expect(request.value(forHTTPHeaderField: "Accept") == "application/json")
    #expect(request.timeoutInterval == 15)
}

@Test func preflightRefusesTrackOrLevelChangesButNotClockProgress() throws {
    let before = try testState()
    let advanced = try fixture(ZoneState.self, stateJSON.replacingOccurrences(of: #""position_secs":32"#, with: #""position_secs":40"#))
    try NativeCommand.seek(60).validate(before: before, current: advanced)
    #expect(throws: ContractError.self) { try NativeCommand.seek(240).validate(before: before, current: advanced) }
    let changed = try fixture(ZoneState.self, stateJSON.replacingOccurrences(of: "test:work:1", with: "test:work:2"))
    #expect(throws: ContractError.self) { try NativeCommand.next.validate(before: before, current: changed) }
    try NativeCommand.queueAdd("spotify:track:EXAMPLE", next: false).validate(before: before, current: changed)
    #expect(throws: ContractError.self) { try NativeCommand.queueAdd("spotify:track:EXAMPLE", next: true).validate(before: before, current: changed) }
    let louder = try fixture(ZoneState.self, stateJSON.replacingOccurrences(of: #""group_volume":24"#, with: #""group_volume":30"#))
    #expect(throws: ContractError.self) { try NativeCommand.groupVolume(25).validate(before: before, current: louder) }
}

@Test func queuePagesValidateAbsolutePositionsAndRemainReadableWithoutRevision() throws {
    let page = try fixture(QueuePage.self, #"{"total":26,"items":[{"position":26,"title":"Last"}]}"#)
    #expect(try page.validated(offset: 25).revision == nil)
    #expect(throws: ContractError.self) { try page.validated(offset: 0) }
}

@Test func wholeWorkFeedbackIsNotATrackSkip() throws {
    let zone = try testZone()
    #expect(try NativeCommand.djSkip.payload(in: zone).path == ["dj", "skip"])
    #expect(try NativeCommand.next.payload(in: zone).path == ["next"])
    #expect(try NativeCommand.djFeedback(like: true).payload(in: zone).body["signal"] == .string("like"))
    #expect(try NativeCommand.djSteer(mood: "Calm", seconds: 3_600).payload(in: zone).body["for_secs"] == .integer(3_600))
}

@Test func scenePartialFailuresAreNotReportedAsSuccess() throws {
    let scene = try fixture(Scene.self, #"{"name":"Evening","saved":1,"groups":[],"volumes":{},"mutes":{}}"#)
    let data = Data(#"{"scene":"Evening","done":"One room failed","changed":true,"complete":false,"steps":["Grouped Studio"],"failed":[{"step":"Set level","error":"Speaker offline"}],"skipped":["Missing room"]}"#.utf8)
    let result = try NativeCommand.scene(scene).receipt(data)
    #expect(result.partial)
    #expect(result.detail.contains("Speaker offline"))
    #expect(result.detail.contains("Skipped: Missing room"))
    #expect(try NativeCommand.scene(scene).payload(in: testZone()).body.isEmpty)
}

@Test func eventParserHandlesChunkBoundariesMultilineUnicodeAndCRLF() throws {
    var parser = EventParser(), events: [ServerEvent] = []
    for byte in Data("\u{feff}: heartbeat\r\nid: 18446744073709551615\r\nevent: zone.state\r\ndata: {\"room\":\"Büro\",\r\ndata: \"volume\":20}\r\n\r\n".utf8) {
        if let event = try parser.feed(byte) { events.append(event) }
    }
    #expect(events.count == 1)
    #expect(events[0].id == "18446744073709551615")
    #expect(events[0].kind == "zone.state")
    #expect(events[0].data == "{\"room\":\"Büro\",\n\"volume\":20}")
}

@Test func eventParserDiscardsUnfinishedFramesAndIgnoresHeartbeatComments() throws {
    var parser = EventParser(), events: [ServerEvent] = []
    for byte in Data(": heartbeat\n\nid: 12\ndata: one\n\nid:\nevent: events.reset\ndata: {}\n\ndata: unfinished".utf8) {
        if let event = try parser.feed(byte) { events.append(event) }
    }
    #expect(events.count == 2)
    #expect(events[0].id == "12")
    #expect(events[1].id == nil)
}

@Test func eventParserEnforcesMemoryBounds() throws {
    var parser = EventParser()
    for _ in 0..<65_536 { _ = try parser.feed(65) }
    #expect(throws: ClientError.self) { try parser.feed(65) }
}


@Test func ordinaryHTTPRepliesAreBoundedBeforeAndDuringReceipt() throws {
    let url = try #require(URL(string: "https://music.example.test/zones"))
    let exact = try #require(HTTPURLResponse(
        url: url, statusCode: 200, httpVersion: "HTTP/1.1",
        headerFields: ["Content-Length": String(URLSessionTransport.maxReplyBytes)]
    ))
    #expect(try URLSessionTransport.checkedHTTP(exact).statusCode == 200)

    let oversized = try #require(HTTPURLResponse(
        url: url, statusCode: 200, httpVersion: "HTTP/1.1",
        headerFields: ["Content-Length": String(URLSessionTransport.maxReplyBytes + 1)]
    ))
    #expect(throws: ClientError.self) { try URLSessionTransport.checkedHTTP(oversized) }

    var data = Data(repeating: 0, count: URLSessionTransport.maxReplyBytes - 1)
    try URLSessionTransport.append(1, to: &data)
    #expect(data.count == URLSessionTransport.maxReplyBytes)
    #expect(throws: ClientError.self) { try URLSessionTransport.append(2, to: &data) }
}
