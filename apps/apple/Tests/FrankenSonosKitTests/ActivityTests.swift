import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

private let activitySource = "spotify:track:0123456789012345678901"
private func activityWire(_ object: Any) throws -> HTTPReply {
    HTTPReply(data: try JSONSerialization.data(withJSONObject: object), status: 200)
}
private func activityAction(id: Int64 = 9_007_199_254_740_993, client: String = "synthetic-client") -> [String: Any] {
    ["id": id, "at": 1_800_000_000, "client": client, "surface": "http", "intent": "set_volume",
     "decision": "clamp: policy", "result": "Volume set to 20", "undoable": true]
}
private func activityPlay(uri: String = activitySource) -> [String: Any] {
    ["room": "Studio", "source_uri": uri, "title": "Test music", "artist": "Test artist", "played_at": 1_800_000_000]
}
private func activityZone() -> Zone {
    Zone(household: "TEST-A", coordinatorRoom: "Studio", members: ["Studio", "Hall"], transportState: "playing")
}
private actor ActivityTransport: DaemonTransport {
    var requests: [URLRequest] = []
    var actionsFailure = false, historyFailure = false
    var revision: Int64 = 1
    var actionOverride: Data?
    var historyOverride: Data?
    var gatePath: String?
    var gate: CheckedContinuation<Void, Never>?
    var blocked = false, inFlight = 0, peak = 0
    func fail(actions: Bool, history: Bool) { actionsFailure = actions; historyFailure = history }
    func bump() { revision += 1 }
    func replaceActions(_ data: Data) { actionOverride = data }
    func replaceHistory(_ data: Data) { historyOverride = data }
    func hold(_ path: String) { gatePath = path }
    func release() { let old = gate; gate = nil; blocked = false; old?.resume() }
    func writes() -> [URLRequest] { requests.filter { $0.httpMethod == "POST" } }
    func send(_ request: URLRequest) async throws -> HTTPReply {
        requests.append(request); inFlight += 1; peak = max(peak, inFlight)
        defer { inFlight -= 1 }
        let path = request.url!.path
        let reply: HTTPReply
        switch path {
        case "/actions":
            if actionsFailure { throw APIProblem(code: "POLICY_DENIED", detail: "Action log unavailable", hint: "Check your client policy") }
            let filter = request.url!.query?.split(separator: "&").first { $0.hasPrefix("client=") }
                .map { String($0.dropFirst(7)).replacingOccurrences(of: "+", with: " ").removingPercentEncoding! }
            reply = try actionOverride.map { HTTPReply(data: $0, status: 200) } ?? activityWire([activityAction(id: revision, client: filter ?? "synthetic-client")])
        case "/history":
            if historyFailure { throw URLError(.cannotConnectToHost) }
            reply = try historyOverride.map { HTTPReply(data: $0, status: 200) } ?? activityWire([activityPlay(), activityPlay()])
        case "/zones":
            let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
            reply = HTTPReply(data: try encoder.encode([activityZone()]), status: 200)
        case let p where p.hasSuffix("/state"):
            reply = try activityWire(["zone": ["household": "TEST-A", "coordinator_room": "Studio", "members": ["Studio", "Hall"], "transport_state": "playing"],
                                      "transport_state": "playing", "volume": 20, "mute": false])
        case "/play": reply = try activityWire(["done": "Playing selected source", "changed": true])
        case "/queue": reply = try activityWire(["done": "Added without starting", "first": 3, "count": 1, "playing_queue": false])
        default: throw ContractError.invalid("Unexpected activity test route")
        }
        if gatePath == path {
            gatePath = nil; blocked = true
            // Simulate a request which completes after cancellation.
            await withCheckedContinuation { gate = $0 }
        }
        return reply
    }
    nonisolated func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> {
        AsyncThrowingStream { $0.finish() }
    }
}
private func activityClient(_ transport: ActivityTransport) throws -> DaemonClient {
    DaemonClient(endpoint: try DaemonEndpoint("https://daemon.example.test"), transport: transport)
}
@MainActor private func activityEventually(_ check: () -> Bool) async throws {
    let end = ContinuousClock.now.advanced(by: .seconds(3))
    while !check() {
        guard ContinuousClock.now < end else { throw ContractError.invalid("Activity did not settle") }
        try await Task.sleep(for: .milliseconds(3))
    }
}
private func activityWaitForGate(_ transport: ActivityTransport) async throws {
    let end = ContinuousClock.now.advanced(by: .seconds(3))
    while !(await transport.blocked) {
        guard ContinuousClock.now < end else { throw ContractError.invalid("Activity gate not reached") }
        try await Task.sleep(for: .milliseconds(3))
    }
}

@Test func activityDecodesFullWidthIDsAndUndoReferences() throws {
    var object = activityAction(); object["undoable"] = false; object["undo_of"] = Int64(9_007_199_254_740_992)
    let row = try DaemonClient.decode(LoggedAction.self, from: activityWire(object).data)
    try row.validate()
    #expect(row.id == 9_007_199_254_740_993 && row.undoOf == 9_007_199_254_740_992)
    #expect(row.decision == "clamp: policy")
    #expect(row.date.timeIntervalSince1970 == 1_800_000_000)
}
@Test func activityHistoryAllowsMissingMetadataAndPreservesDuplicates() async throws {
    let transport = ActivityTransport(), client = try activityClient(transport)
    let minimal: [String: Any] = ["source_uri": activitySource, "played_at": 1_800_000_000]
    await transport.replaceHistory(try activityWire([minimal, minimal]).data)
    let rows = try await client.recentPlays()
    #expect(rows.count == 2 && rows[0] == rows[1])
    #expect(rows[0].room == nil && rows[0].artist == nil && rows[0].displayTitle == "Recorded source")
}
@Test func activityQueriesKeepClientAndHouseholdNamesAsData() async throws {
    let transport = ActivityTransport(), client = try activityClient(transport)
    let query = ActivityQuery(client: "test+client&limit=999", since: 1_799_999_900,
                              historyRoom: RoomID(household: "TEST-B", room: "Studio & Hall/2"), limit: 25)
    _ = try await client.recentActions(query); _ = try await client.recentPlays(query)
    let requests = await transport.requests
    #expect(requests.map { $0.url!.path } == ["/actions", "/history"])
    let actions = URLComponents(url: requests[0].url!, resolvingAgainstBaseURL: false)!.queryItems!
    let history = URLComponents(url: requests[1].url!, resolvingAgainstBaseURL: false)!.queryItems!
    #expect(actions == query.actionItems && history == query.historyItems)
    #expect(actions.count == 3 && history.count == 2)
    #expect(requests.allSatisfy { $0.httpBody == nil && $0.httpMethod == "GET" && $0.url!.host == "daemon.example.test" })
}
@Test func activityInvalidFiltersNeverDispatchReads() async throws {
    let transport = ActivityTransport(), client = try activityClient(transport)
    for query in [ActivityQuery(limit: 0), ActivityQuery(limit: 201), ActivityQuery(client: " "),
                  ActivityQuery(since: Int64.max), ActivityQuery(historyRoom: RoomID(household: "", room: "Studio"))] {
        await #expect(throws: ContractError.self) { try await client.recentActions(query) }
        await #expect(throws: ContractError.self) { try await client.recentPlays(query) }
    }
    #expect(await transport.requests.isEmpty)
}
@Test func activityRejectsDuplicateActionIDsAndFilterMismatch() async throws {
    let transport = ActivityTransport(), client = try activityClient(transport)
    await transport.replaceActions(try activityWire([activityAction(), activityAction()]).data)
    await #expect(throws: ContractError.self) { try await client.recentActions() }
    await transport.replaceActions(try activityWire([activityAction()]).data)
    await #expect(throws: ContractError.self) { try await client.recentActions(ActivityQuery(client: "another-client")) }
    await #expect(throws: ContractError.self) { try await client.recentActions(ActivityQuery(since: 1_800_000_001)) }
}
@Test func activityRejectsExcessRowsAndContradictoryActions() async throws {
    let transport = ActivityTransport(), client = try activityClient(transport)
    await #expect(throws: ContractError.self) { try await client.recentPlays(ActivityQuery(limit: 1)) }
    var row = activityAction(); row["undo_of"] = 3
    await transport.replaceActions(try activityWire([row]).data)
    await #expect(throws: ContractError.self) { try await client.recentActions() }
}
@Test func activityRejectsInvalidDatesAndIncompleteHistory() async throws {
    let transport = ActivityTransport(), client = try activityClient(transport)
    for value in ["missing", "empty", "extreme"] {
        var row = activityPlay()
        if value == "missing" { row.removeValue(forKey: "source_uri") }
        else if value == "empty" { row["source_uri"] = "" }
        else { row["played_at"] = Int64.max }
        await transport.replaceHistory(try activityWire([row]).data)
        await #expect(throws: (any Error).self) { try await client.recentPlays() }
    }
}
@Test func activityPortableSourcesDoNotBorrowRendererContext() throws {
    for uri in [activitySource, "spotify:album:0123456789012345678901", "https://media.example.test/stream?q=two%20words"] {
        let play = try DaemonClient.decode(RecordedPlay.self, from: activityWire(activityPlay(uri: uri)).data)
        #expect(play.replaySource == uri)
    }
    for uri in ["x-sonos-spotify:spotify%3atrack%3atest?sid=1", "x-rincon:TEST-PLAYER", "file:///tmp/audio.wav",
                "https://user:password@media.example.test/", "https://media.example.test/#fragment", "spotify:track:short", " https://media.example.test", "https://media.example.test/\n"] {
        let play = try DaemonClient.decode(RecordedPlay.self, from: activityWire(activityPlay(uri: uri)).data)
        #expect(play.replaySource == nil)
    }
}
@Test @MainActor func activitySessionReadsOnlyWhenOpenedAndDoesNotPoll() async throws {
    let transport = ActivityTransport(), model = try ActivityModel(client: activityClient(transport))
    defer { model.stop() }
    #expect(await transport.requests.isEmpty)
    model.resume(); model.resume()
    try await activityEventually { model.historyObservedAt != nil && !model.loading }
    #expect(model.actions.count == 1 && model.history.count == 2)
    #expect(Set(model.history.map(\.id)).count == 2)
    try await Task.sleep(for: .milliseconds(75))
    #expect(await transport.requests.count == 2)
    #expect(await transport.writes().isEmpty)
}
@Test @MainActor func activityFailedActionFeedDoesNotEraseHistory() async throws {
    let transport = ActivityTransport()
    await transport.fail(actions: true, history: false)
    let model = try ActivityModel(client: activityClient(transport)); defer { model.stop() }
    model.resume()
    try await activityEventually { !model.loading }
    #expect(model.actionsObservedAt == nil && model.actionsStale)
    #expect(model.actionsProblem?.contains("Check your client policy") == true)
    #expect(model.history.count == 2 && !model.historyStale)
}
@Test @MainActor func activityFailedRefreshRetainsClearlyStaleHistoryUntilRecovery() async throws {
    let transport = ActivityTransport(), model = try ActivityModel(client: activityClient(transport)); defer { model.stop() }
    model.resume(); try await activityEventually { !model.loading }
    let id = try #require(model.history.first?.id), observed = model.historyObservedAt
    await transport.fail(actions: false, history: true)
    model.refresh(); try await activityEventually { !model.loading }
    #expect(model.history.count == 2 && model.historyObservedAt == observed && model.historyStale)
    #expect(throws: ContractError.self) { try model.replayCommand(id, as: .play) }
    #expect(!model.actionsStale)
    await transport.fail(actions: false, history: false)
    model.refresh(); try await activityEventually { !model.loading }
    #expect(!model.historyStale && model.historyProblem == nil)
    #expect(throws: ContractError.self) { try model.replayCommand(id, as: .play) }
}
@Test @MainActor func activityChangedQueryFencesLateOldResultsAndBoundsConcurrency() async throws {
    let transport = ActivityTransport(), model = try ActivityModel(client: activityClient(transport)); defer { model.stop() }
    await transport.hold("/history"); model.resume()
    try await activityWaitForGate(transport)
    try await activityEventually { model.actionsObservedAt != nil }
    #expect(model.actions.count == 1) // Slow history does not hold the actions hostage.
    model.apply(ActivityQuery(client: "new-client", historyRoom: RoomID(household: "TEST-B", room: "Studio")))
    #expect(model.history.isEmpty && model.actions.isEmpty)
    await transport.release()
    try await activityEventually { !model.loading && model.historyObservedAt != nil }
    #expect(model.actions.first?.client == "new-client")
    #expect(await transport.peak <= 2)
    #expect(await transport.requests.count == 4)
}
@Test @MainActor func activitySuspendAndImmediateReopenDrainOldReads() async throws {
    let transport = ActivityTransport(), model = try ActivityModel(client: activityClient(transport)); defer { model.stop() }
    await transport.hold("/history"); model.resume(); try await activityWaitForGate(transport)
    model.suspend(); model.resume(); await transport.bump()
    #expect(model.loading)
    await transport.release()
    try await activityEventually { !model.loading && model.historyObservedAt != nil }
    #expect(model.actions.first?.id == 2 && !model.historyStale)
    #expect(await transport.peak <= 2)
}
@Test @MainActor func activityRefreshBurstsCoalesceToOneFollowingBatch() async throws {
    let transport = ActivityTransport(), model = try ActivityModel(client: activityClient(transport)); defer { model.stop() }
    await transport.hold("/history"); model.resume(); try await activityWaitForGate(transport)
    for _ in 0..<100 { model.refresh() }
    await transport.release(); try await activityEventually { !model.loading }
    #expect(await transport.requests.count == 4)
}
@Test @MainActor func activityStoppingIsTerminalAndRejectsSelections() async throws {
    let transport = ActivityTransport(), model = try ActivityModel(client: activityClient(transport))
    model.resume(); try await activityEventually { !model.loading }
    let id = try #require(model.history.first?.id)
    model.stop(); model.resume(); model.refresh(); model.apply(ActivityQuery(limit: 100))
    #expect(!model.active)
    #expect(throws: ContractError.self) { try model.replayCommand(id, as: .play) }
    #expect(await transport.requests.count == 2)
}
@Test @MainActor func activityBadFilterKeepsPreviousObservationAndNeverBroadensQuery() async throws {
    let transport = ActivityTransport(), model = try ActivityModel(client: activityClient(transport)); defer { model.stop() }
    model.apply(ActivityQuery(client: "original-client")); model.resume()
    try await activityEventually { !model.loading }
    model.apply(ActivityQuery(client: "", limit: 200))
    #expect(model.filterProblem != nil && model.query.client == "original-client")
    #expect(model.actions.first?.client == "original-client")
    #expect(await transport.requests.count == 2)
}
@Test @MainActor func activitySourcesBecomeExistingCommandsOnlyAfterExplicitSelection() async throws {
    let transport = ActivityTransport(), client = try activityClient(transport), model = ActivityModel(client: client)
    defer { model.stop() }
    model.resume(); try await activityEventually { !model.loading }
    let id = try #require(model.history.first?.id)
    #expect(try model.replayCommand(id, as: .play) == .playSource(activitySource))
    #expect(try model.replayCommand(id, as: .queueNext) == .queueAdd(activitySource, next: true))
    let append = try model.replayCommand(id, as: .queueLast)
    #expect(append == .queueAdd(activitySource, next: false))
    #expect(await transport.writes().isEmpty)
    let commands = CommandReviewSession(client: client)
    let review = try await commands.prepare(append, in: activityZone())
    #expect(await transport.writes().isEmpty)
    #expect(try await commands.execute(review).title == "Added without starting")
    await #expect(throws: ContractError.self) { try await commands.execute(review) }
    let writes = await transport.writes()
    #expect(writes.count == 1 && writes[0].url!.path == "/queue")
    let body = try JSONDecoder().decode([String: JSONValue].self, from: writes[0].httpBody!)
    #expect(body == ["zone": .string("Studio@TEST-A"), "source_uri": .string(activitySource), "next": .bool(false)])
}
@Test @MainActor func activityFiltersAndSuspensionInvalidateReplayTokens() async throws {
    let transport = ActivityTransport(), model = try ActivityModel(client: activityClient(transport)); defer { model.stop() }
    model.resume(); try await activityEventually { !model.loading }
    let id = try #require(model.history.first?.id)
    model.refresh()
    #expect(throws: ContractError.self) { try model.replayCommand(id, as: .play) }
    try await activityEventually { !model.loading }
    #expect(throws: ContractError.self) { try model.replayCommand(id, as: .play) }
    let fresh = try #require(model.history.first?.id)
    model.suspend()
    #expect(throws: ContractError.self) { try model.replayCommand(fresh, as: .play) }
}


@Test func activityQueryEncodingMatchesDaemonFormDecodingAcrossReadRoutes() throws {
    let endpoint = try DaemonEndpoint("https://daemon.example.test")
    for path in [["actions"], ["history"], ["library", "search"], ["sleep"]] {
        let value = "Test+Music & More/%2B=日本語"
        let url = try endpoint.resource(path, query: [URLQueryItem(name: "q", value: value)])
        let raw = try #require(url.query?.split(separator: "&").first)
        let decoded = String(raw.dropFirst(2)).replacingOccurrences(of: "+", with: " ").removingPercentEncoding
        #expect(decoded == value)
        #expect(url.query?.contains("+") == false)
    }
}
