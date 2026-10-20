import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

private let filtersZoneJSON = #"{"household":"S2","coordinator_room":"Studio","members":["Studio"],"transport_state":"playing"}"#
private let filtersSteeringJSON = #"{"source":"session","mood":"calm","added":{"include_genres":["jazz"]},"expires_at":2000000000,"expires_in_secs":3600}"#
private actor FilterTransport: DaemonTransport {
    private(set) var requests: [URLRequest] = []
    var steering = filtersSteeringJSON
    var mood = #"{"name":"calm","constraints":{"energy_bias":-1}}"#
    var track = "test:first"
    var answer = #"{"done":"Temporary steering saved","changed":true}"#
    func configure(steering: String? = nil, mood: String? = nil, track: String? = nil, answer: String? = nil) {
        if let steering { self.steering = steering }; if let mood { self.mood = mood }
        if let track { self.track = track }; if let answer { self.answer = answer }
    }
    func send(_ request: URLRequest) async throws -> HTTPReply {
        requests.append(request)
        let json: String
        switch (request.httpMethod ?? "GET", request.url?.path ?? "") {
        case ("GET", "/zones"): json = "[\(filtersZoneJSON)]"
        case ("GET", let path) where path.hasSuffix("/state"):
            json = "{\"zone\":\(filtersZoneJSON),\"transport_state\":\"playing\",\"volume\":20,\"mute\":false,\"track\":{\"uri\":\"\(track)\"}}"
        case ("GET", let path) where path.hasSuffix("/dj"):
            // Match serde's real stopped-session shape: no now and no next.
            json = #"{"zone":"Studio","running":false,"steering":STEERING}"#.replacingOccurrences(of: "STEERING", with: steering)
        case ("GET", "/dj/moods"): json = "{\"moods\":[\(mood)]}"
        case ("POST", "/dj/steer"): json = answer
        default: throw ContractError.invalid("Unexpected request: \(request.url?.path ?? "")")
        }
        return HTTPReply(data: Data(json.utf8), status: 200)
    }
    nonisolated func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> {
        AsyncThrowingStream { $0.finish() }
    }
}
private func filterFixture(_ transport: FilterTransport) throws -> (CommandReviewSession, NativeCommand, Zone) {
    let client = DaemonClient(endpoint: try DaemonEndpoint("https://music.example.test"), transport: transport)
    let zone = try DaemonClient.decode(Zone.self, from: Data(filtersZoneJSON.utf8))
    let steering = try DaemonClient.decode(DJStatus.Steering.self, from: Data(filtersSteeringJSON.utf8))
    let mood = MoodCatalog.Mood(name: "calm", constraints: ["energy_bias": .integer(-1)])
    let change = try DJSteeringChange(in: zone, before: steering, mood: mood,
        filters: DJSteeringFilters(["include_artists": .array([.string("Example artist")])]), seconds: 7200)
    return (CommandReviewSession(client: client), .djFilters(change), zone)
}

@Test func nativeFiltersUseTheExistingSingleShotSteerPath() async throws {
    let transport = FilterTransport()
    let (session, command, zone) = try filterFixture(transport)
    #expect(command.requiresDJ)
    let review = try await session.prepare(command, in: zone)
    #expect(await transport.requests.allSatisfy { ($0.httpMethod ?? "GET") == "GET" })
    let receipt = try await session.execute(review)
    #expect(receipt.title == "Temporary steering saved")
    let writes = await transport.requests.filter { $0.httpMethod == "POST" }
    #expect(writes.count == 1)
    #expect(writes[0].url?.path == "/dj/steer")
    #expect(try JSONDecoder().decode([String: JSONValue].self, from: writes[0].httpBody!) == [
        "zone": .string("Studio@S2"), "mood": .string("calm"), "for_secs": .integer(7200),
        "constraints": .object(["include_artists": .array([.string("Example artist")])])
    ])
    await #expect(throws: ContractError.self) { try await session.execute(review) }
    #expect(await transport.requests.filter { $0.httpMethod == "POST" }.count == 1)
}

@Test func nativeFilterDraftAndReviewedSessionChangesStopBeforeWriting() async throws {
    for afterReview in [false, true] {
        let transport = FilterTransport()
        let (session, command, zone) = try filterFixture(transport)
        let review = afterReview ? try await session.prepare(command, in: zone) : nil
        await transport.configure(steering: filtersSteeringJSON.replacingOccurrences(of: "2000000000", with: "2000000001"))
        if let review { await #expect(throws: ContractError.self) { try await session.execute(review) } }
        else { await #expect(throws: ContractError.self) { try await session.prepare(command, in: zone) } }
        #expect(await transport.requests.allSatisfy { ($0.httpMethod ?? "GET") == "GET" })
    }
}

@Test func nativeFiltersAllowTrackProgressWithoutChangingTheirSteeringBaseline() async throws {
    let transport = FilterTransport()
    let (session, command, zone) = try filterFixture(transport)
    let review = try await session.prepare(command, in: zone)
    await transport.configure(steering: filtersSteeringJSON.replacingOccurrences(of: ":3600", with: ":3550"), track: "test:second")
    _ = try await session.execute(review)
    #expect(await transport.requests.filter { $0.httpMethod == "POST" }.count == 1)
}

@Test func nativeFiltersRecheckSelectedMoodDefinitionsBeforeSending() async throws {
    let transport = FilterTransport()
    let (session, command, zone) = try filterFixture(transport)
    let review = try await session.prepare(command, in: zone)
    await transport.configure(mood: #"{"name":"calm","constraints":{"energy_bias":2}}"#)
    await #expect(throws: ContractError.self) { try await session.execute(review) }
    #expect(await transport.requests.allSatisfy { ($0.httpMethod ?? "GET") == "GET" })
}

@Test func cancellingNativeFilterReviewCannotSendItLater() async throws {
    let transport = FilterTransport()
    let (session, command, zone) = try filterFixture(transport)
    let review = try await session.prepare(command, in: zone)
    await session.cancel()
    await #expect(throws: ContractError.self) { try await session.execute(review) }
    #expect(await transport.requests.allSatisfy { ($0.httpMethod ?? "GET") == "GET" })
}

@Test func malformedFilterAcknowledgementsRemainUncertainAndNeverRepeat() async throws {
    let transport = FilterTransport()
    let (session, command, zone) = try filterFixture(transport)
    let review = try await session.prepare(command, in: zone)
    await transport.configure(answer: "not JSON")
    do { _ = try await session.execute(review); Issue.record("Expected uncertain result") }
    catch let error as ClientError {
        guard case .uncertain = error else { Issue.record("Expected uncertainty, got \(error)"); return }
    }
    await #expect(throws: ContractError.self) { try await session.execute(review) }
    #expect(await transport.requests.filter { $0.httpMethod == "POST" }.count == 1)
}
