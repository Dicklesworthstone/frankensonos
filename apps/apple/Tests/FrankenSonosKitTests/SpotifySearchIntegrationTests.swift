import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

private let searchedSourceID = "abcdefghijklmnopqrstuv"
private func searchedZone(_ house: String) -> Zone {
    Zone(household: house, coordinatorRoom: "Studio", members: ["Studio"], transportState: "playing")
}

/// Use the production search client and command actor, with controlled daemon
/// observations. The catalog is read once; confirmation retains its exact URI.
private actor SearchedSourceTransport: DaemonTransport {
    let house: String
    var requests: [URLRequest] = []
    var changedGroup = false, lostReply = false
    var holdState = false, waiting = false
    private var gate: CheckedContinuation<Void, Never>?
    init(house: String) { self.house = house }
    func changeGroup() { changedGroup = true }
    func loseReply() { lostReply = true }
    func holdNextState() { holdState = true }
    func release() { let old = gate; gate = nil; waiting = false; old?.resume() }

    func send(_ request: URLRequest) async throws -> HTTPReply {
        requests.append(request)
        let path = request.url!.path.removingPercentEncoding ?? request.url!.path
        let zone: [String: Any] = ["household": house, "coordinator_room": "Studio",
                                   "members": changedGroup ? ["Studio", "Hall"] : ["Studio"], "transport_state": "playing"]
        let body: Any
        if request.httpMethod == "POST" {
            guard path == "/play" || path == "/queue" else { throw ContractError.invalid("Unexpected mutation") }
            if lostReply { throw URLError(.networkConnectionLost) }
            body = path == "/play" ? ["done": "Playing selected source", "changed": true]
                : ["done": "Queued selected source", "first": 3, "count": 1, "playing_queue": true]
        } else {
            switch path {
            case "/spotify/search":
                let parameters = URLComponents(url: request.url!, resolvingAgainstBaseURL: false)!.queryItems!
                let query = parameters.first { $0.name == "q" }!.value!
                let kind = parameters.first { $0.name == "kind" }!.value!
                body = ["query": query, "kind": kind, "more": false,
                        "items": [["source_uri": "spotify:\(kind):\(searchedSourceID)", "title": "Chosen \(kind)", "subtitle": "Fixture catalog"]]]
            case "/zones": body = [zone]
            case "/zones/Studio@\(house)/state":
                if holdState {
                    holdState = false; waiting = true
                    await withCheckedContinuation { gate = $0 }
                }
                body = ["zone": zone, "transport_state": "playing", "volume": 20, "mute": false,
                        "track": ["uri": "test:current", "queue_position": 1, "duration_secs": 120, "position_secs": 10]]
            case "/zones/Studio@\(house)/queue":
                body = ["zone": "Studio", "total": 2, "revision": "fixture-queue-v1", "playing": 1,
                        "items": [["position": 1, "title": "Current", "uri": "test:current"]]]
            default: throw ContractError.invalid("Unexpected read: \(path)")
            }
        }
        return HTTPReply(data: try JSONSerialization.data(withJSONObject: body), status: 200)
    }
    nonisolated func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> {
        AsyncThrowingStream { $0.finish() }
    }
}

@MainActor private func searchedModel(_ transport: SearchedSourceTransport, kind: SpotifyCatalogKind = .track) async throws -> (SpotifySearchModel, DaemonClient) {
    let client = DaemonClient(endpoint: try DaemonEndpoint("https://music.example.test"), transport: transport)
    let model = SpotifySearchModel(client: client)
    model.resume(); model.edit("fixture"); model.chooseKind(kind); model.search()
    let deadline = ContinuousClock.now.advanced(by: .seconds(3))
    while model.loading {
        guard ContinuousClock.now < deadline else { throw URLError(.timedOut) }
        try await Task.sleep(for: .milliseconds(2))
    }
    _ = try #require(model.observation)
    return (model, client)
}

@Suite("Native Spotify search command handoff") @MainActor struct SpotifySearchIntegrationTests {
    @Test(arguments: SpotifyCatalogKind.allCases, ["S1", "S2"])
    func allKindsUseExistingOneUseReviewForEachExplicitAction(kind: SpotifyCatalogKind, house: String) async throws {
        for action in SpotifySearchSelection.Action.allCases {
            let transport = SearchedSourceTransport(house: house)
            let (model, client) = try await searchedModel(transport, kind: kind)
            defer { model.stop() }
            let observed = try #require(model.observation)
            #expect(await transport.requests.count == 1)
            let selected = try model.selection(observed.page.items[0], from: observed.id, in: searchedZone(house), action: action)
            #expect(await transport.requests.count == 1, "Choosing creates no read or write")
            let session = CommandReviewSession(client: client)
            let review = try await session.prepare(selected.command, in: selected.zone)
            #expect(await transport.requests.allSatisfy { $0.httpMethod == "GET" })
            model.edit("another query")
            let receipt = try await session.execute(review)
            #expect(receipt.title == (action == .playNow ? "Playing selected source" : "Queued selected source"))
            await #expect(throws: ContractError.self) { try await session.execute(review) }
            let requests = await transport.requests, writes = requests.filter { $0.httpMethod == "POST" }
            #expect(writes.count == 1)
            let write = try #require(writes.first)
            let payload = try JSONDecoder().decode([String: JSONValue].self, from: #require(write.httpBody))
            var expected: [String: JSONValue] = ["zone": .string("Studio@\(house)"),
                                                "source_uri": .string("spotify:\(kind.rawValue):\(searchedSourceID)")]
            if action != .playNow { expected["next"] = .bool(action == .queueNext) }
            #expect(payload == expected)
            #expect(write.url?.path == (action == .playNow ? "/play" : "/queue"))
            #expect(requests.filter { $0.url?.path == "/spotify/search" }.count == 1,
                    "Confirmation must not select a different search result")
        }
    }

    @Test func aChangedDestinationGroupCannotReceiveTheReviewedSearchResult() async throws {
        let transport = SearchedSourceTransport(house: "S2")
        let (model, client) = try await searchedModel(transport, kind: .artist)
        defer { model.stop() }
        let observed = try #require(model.observation)
        let selected = try model.selection(observed.page.items[0], from: observed.id, in: searchedZone("S2"), action: .playNow)
        let session = CommandReviewSession(client: client)
        let review = try await session.prepare(selected.command, in: selected.zone)
        await transport.changeGroup()
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.requests.allSatisfy { $0.httpMethod == "GET" })
    }

    @Test func cancellationDuringFinalStateReadNeverDispatchesTheCatalogSource() async throws {
        let transport = SearchedSourceTransport(house: "S1")
        let (model, client) = try await searchedModel(transport, kind: .show)
        defer { model.stop() }
        let observed = try #require(model.observation)
        let selected = try model.selection(observed.page.items[0], from: observed.id, in: searchedZone("S1"), action: .queueNext)
        let session = CommandReviewSession(client: client)
        let review = try await session.prepare(selected.command, in: selected.zone)
        await transport.holdNextState()
        let sending = Task { try await session.execute(review) }
        let deadline = ContinuousClock.now.advanced(by: .seconds(3))
        while !(await transport.waiting) {
            guard ContinuousClock.now < deadline else { throw URLError(.timedOut) }
            try await Task.sleep(for: .milliseconds(2))
        }
        sending.cancel(); await session.cancel(); await transport.release()
        await #expect(throws: CancellationError.self) { try await sending.value }
        #expect(await transport.requests.allSatisfy { $0.httpMethod == "GET" })
    }

    @Test func aLostPlaybackReplyStaysUncertainAndCannotReplay() async throws {
        let transport = SearchedSourceTransport(house: "S2")
        let (model, client) = try await searchedModel(transport, kind: .episode)
        defer { model.stop() }
        let observed = try #require(model.observation)
        let selected = try model.selection(observed.page.items[0], from: observed.id, in: searchedZone("S2"), action: .playNow)
        let session = CommandReviewSession(client: client)
        let review = try await session.prepare(selected.command, in: selected.zone)
        await transport.loseReply()
        do { _ = try await session.execute(review); Issue.record("Unexpected confirmed playback") }
        catch let failure as ClientError { guard case .uncertain = failure else { throw failure } }
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.requests.filter { $0.httpMethod == "POST" }.count == 1)
    }
}
