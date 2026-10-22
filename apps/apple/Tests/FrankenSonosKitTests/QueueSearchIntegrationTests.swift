import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

/// Exercise the real search-to-review handoff, not a fabricated consecutive
/// queue page. The same renderer URI occurs at positions 3 and 51.
private actor QueueSearchHandoffTransport: DaemonTransport {
    let house: String
    var revision = "queue:1"
    var requests: [URLRequest] = []
    var lostReply = false
    init(house: String) { self.house = house }
    func changeQueue() { revision = "queue:2" }
    func loseReply() { lostReply = true }
    func send(_ request: URLRequest) async throws -> HTTPReply {
        requests.append(request)
        let zone: [String: Any] = ["household": house, "coordinator_room": "Den", "members": ["Den"], "transport_state": "playing"]
        let item: [String: Any] = ["position": 51, "title": "Repeated recording", "uri": "fixture:same"]
        let body: Any
        if request.httpMethod == "POST" {
            guard request.url?.path == "/queue/play" else { throw ContractError.invalid("Unexpected write") }
            if lostReply { throw URLError(.networkConnectionLost) }
            body = ["done": "Playing occurrence 51", "changed": true]
        } else {
            switch request.url?.path {
            case "/zones": body = [zone]
            case "/zones/Den@\(house)/queue/search":
                body = ["zone": "Den", "query": "recording", "revision": revision, "total": 60,
                        "offset": 0, "examined": 60, "complete": true,
                        "items": [["position": 3, "title": "Repeated recording", "uri": "fixture:same"], item]]
            case "/zones/Den@\(house)/state":
                body = ["zone": zone, "transport_state": "playing", "volume": 20, "mute": false,
                        "track": ["uri": "fixture:current", "queue_position": 1, "position_secs": 10, "duration_secs": 120]]
            case "/zones/Den@\(house)/queue":
                body = ["total": 60, "revision": revision, "items": [item], "playing": 1]
            default: throw ContractError.invalid("Unexpected read")
            }
        }
        return HTTPReply(data: try JSONSerialization.data(withJSONObject: body), status: 200)
    }
    nonisolated func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> {
        AsyncThrowingStream { $0.finish() }
    }
}

@MainActor private func handoffSearch(_ transport: QueueSearchHandoffTransport, house: String) async throws -> (QueueSearchModel, DaemonClient) {
    let client = DaemonClient(endpoint: try DaemonEndpoint("https://music.example.test"), transport: transport)
    let model = QueueSearchModel(client: client)
    model.activate(in: Zone(household: house, coordinatorRoom: "Den", members: ["Den"], transportState: "playing"))
    model.edit("recording"); model.search()
    let deadline = ContinuousClock.now.advanced(by: .seconds(3))
    while model.loading {
        guard ContinuousClock.now < deadline else { throw ContractError.invalid("Search did not settle") }
        try await Task.sleep(for: .milliseconds(5))
    }
    _ = try #require(model.observation)
    return (model, client)
}

@Test(arguments: ["S1", "S2"]) @MainActor
func searchedOccurrenceUsesExistingOneUseQueueReview(house: String) async throws {
    let transport = QueueSearchHandoffTransport(house: house)
    let (model, client) = try await handoffSearch(transport, house: house)
    defer { model.stop() }
    let observed = try #require(model.observation)
    let selected = try model.selection(observed.page.items[1], from: observed.id)
    let session = CommandReviewSession(client: client)
    let review = try await session.prepare(selected.command, in: selected.zone)
    #expect(await transport.requests.allSatisfy { $0.httpMethod == "GET" })
    let receipt = try await session.execute(review)
    #expect(receipt.title == "Playing occurrence 51")
    await #expect(throws: ContractError.self) { try await session.execute(review) }
    let requests = await transport.requests
    let writes = requests.filter { $0.httpMethod == "POST" }
    #expect(writes.count == 1 && writes.first?.url?.path == "/queue/play")
    let data = try #require(writes.first?.httpBody)
    let body = try JSONDecoder().decode([String: JSONValue].self, from: data)
    #expect(body == ["zone": .string("Den@\(house)"), "position": .integer(51),
                     "expected_uri": .string("fixture:same"), "expected_revision": .string("queue:1")])
    #expect(requests.filter { $0.httpMethod == "GET" }.count == 8)
}

@Test @MainActor func searchedOccurrenceCannotFollowAChangedRevisionAfterReview() async throws {
    let transport = QueueSearchHandoffTransport(house: "S2")
    let (model, client) = try await handoffSearch(transport, house: "S2")
    defer { model.stop() }
    let observed = try #require(model.observation)
    let selected = try model.selection(observed.page.items[1], from: observed.id)
    let session = CommandReviewSession(client: client)
    let review = try await session.prepare(selected.command, in: selected.zone)
    await transport.changeQueue()
    await #expect(throws: ContractError.self) { try await session.execute(review) }
    #expect(await transport.requests.allSatisfy { $0.httpMethod == "GET" })
}

@Test @MainActor func searchedOccurrenceLostPlaybackReplyIsNotReplayedBySearch() async throws {
    let transport = QueueSearchHandoffTransport(house: "S2")
    let (model, client) = try await handoffSearch(transport, house: "S2")
    defer { model.stop() }
    let observed = try #require(model.observation)
    let selected = try model.selection(observed.page.items[1], from: observed.id)
    let session = CommandReviewSession(client: client)
    let review = try await session.prepare(selected.command, in: selected.zone)
    await transport.loseReply()
    do { _ = try await session.execute(review); Issue.record("Unexpected success") }
    catch let error as ClientError {
        guard case .uncertain = error else { throw error }
    }
    model.invalidate(); model.search()
    let deadline = ContinuousClock.now.advanced(by: .seconds(3))
    while model.loading {
        guard ContinuousClock.now < deadline else { throw ContractError.invalid("Search did not settle") }
        try await Task.sleep(for: .milliseconds(5))
    }
    await #expect(throws: ContractError.self) { try await session.execute(review) }
    #expect(await transport.requests.filter { $0.httpMethod == "POST" }.count == 1)
}
