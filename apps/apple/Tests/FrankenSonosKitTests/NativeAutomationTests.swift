import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

// Synthetic daemon wire replies. Requests still traverse the production
// client, codecs, bounded room reader, command actor and screen controller.
actor NativeAutomationTransport: DaemonTransport {
    var requests: [URLRequest] = []
    var definition = automationScene()
    var list: [Scene]?
    var failPath: String?
    var wrongSceneName = false
    var partial = false
    var lostWrite = false
    var malformedWrite = false
    var changedLevel = false
    private var holdPath: String?
    private var holdMethod = "GET"
    private var releaseRead: CheckedContinuation<Void, Never>?
    var blocked = false

    func replaceScene(_ scene: Scene) { definition = scene }
    func replaceList(_ scenes: [Scene]) { list = scenes }
    func fail(_ path: String?) { failPath = path }
    func wrongName() { wrongSceneName = true }
    func partialResult() { partial = true }
    func loseWrite() { lostWrite = true }
    func breakWrite() { malformedWrite = true }
    func changeLevel() { changedLevel = true }
    func hold(_ path: String, method: String = "GET") { holdPath = path; holdMethod = method; blocked = false }
    func release() { let continuation = releaseRead; releaseRead = nil; blocked = false; continuation?.resume() }
    func writes() -> [URLRequest] { requests.filter { $0.httpMethod != nil && $0.httpMethod != "GET" } }

    func send(_ request: URLRequest) async throws -> HTTPReply {
        requests.append(request)
        let path = request.url!.path.removingPercentEncoding ?? request.url!.path
        if failPath == path { throw URLError(.cannotConnectToHost) }
        let reply = try answer(request, path: path)
        if holdPath == path && (request.httpMethod ?? "GET") == holdMethod {
            holdPath = nil; blocked = true
            // Deliberately allow a late result from a cancellation-ignoring
            // transport; production must keep it from updating a new screen.
            await withCheckedContinuation { releaseRead = $0 }
        }
        return reply
    }

    private func answer(_ request: URLRequest, path: String) throws -> HTTPReply {
        if request.httpMethod == "POST" {
            if lostWrite { throw URLError(.networkConnectionLost) }
            if malformedWrite { return HTTPReply(data: Data("incomplete".utf8), status: 200) }
            if path.hasPrefix("/scenes/") && path.hasSuffix("/apply") {
                return try json(SceneReport(scene: definition.name, done: partial ? "Some steps failed" : "Scene applied",
                    changed: true, complete: !partial, steps: ["Grouped Workshop@TEST-B"],
                    failed: partial ? [.init(step: "Set level", error: "Speaker unavailable")] : nil,
                    skipped: partial ? ["Missing room"] : nil,
                    notes: partial ? [.init(code: "VOLUME_CLAMPED", detail: "Capped at 25")] : nil))
            }
            return try json(Outcome(done: "Applied", changed: true, volume: nil, notes: nil))
        }
        if path == "/zones" { return try json([automationZone(), automationZone(house: "TEST-B")]) }
        if path.hasSuffix("/state") {
            let zone = path.contains("TEST-B") ? automationZone(house: "TEST-B") : automationZone()
            return try json(ZoneState(zone: zone, transportState: "playing", volume: changedLevel ? 30 : 20,
                mute: false, groupVolume: 20,
                track: Track(uri: "test:source", title: "Test track", creator: nil, album: nil, durationSecs: 300,
                             positionSecs: 20, queuePosition: 1), shuffle: false, repeat: "none"))
        }
        if path.hasSuffix("/queue") { return try json(QueuePage(total: 0, items: [], playing: nil, revision: "test-r1")) }
        if path == "/scenes" { return try json(list ?? [definition]) }
        if path.hasPrefix("/scenes/") { return try json(wrongSceneName ? automationScene(name: "Another scene") : definition) }
        if path == "/favorites" { return try json([Favorite]()) }
        throw ContractError.invalid("Unexpected test path: \(path)")
    }
    nonisolated func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> {
        AsyncThrowingStream { $0.yield(.connected) }
    }
    private func json<T: Encodable>(_ value: T) throws -> HTTPReply {
        let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
        return HTTPReply(data: try encoder.encode(value), status: 200)
    }
}

func automationZone(house: String = "TEST-A") -> Zone {
    Zone(household: house, coordinatorRoom: "Workshop", members: ["Workshop", "Hall"], transportState: "playing")
}
func automationScene(name: String = "Dinner / café & friends", level: Int = 25, saved: Int64 = 1) -> Scene {
    Scene(name: name, saved: saved,
          groups: [.init(coordinator: "Workshop@TEST-B", members: ["Hall@TEST-B"],
                         source: .init(kind: "favorite", name: "Test station", uri: nil, mood: nil), playing: true)],
          volumes: ["Hall@TEST-B": level], mutes: ["Workshop@TEST-B": false])
}
func automationClient(_ transport: NativeAutomationTransport) throws -> DaemonClient {
    DaemonClient(endpoint: try DaemonEndpoint("https://daemon.example.test"), transport: transport)
}
func waitForAutomationRead(_ transport: NativeAutomationTransport) async throws {
    let deadline = ContinuousClock.now.advanced(by: .seconds(3))
    while !(await transport.blocked) {
        guard ContinuousClock.now < deadline else { throw ContractError.invalid("The held request never arrived") }
        try await Task.sleep(for: .milliseconds(5))
    }
}
@MainActor func automationEventually(_ condition: () -> Bool) async throws {
    let deadline = ContinuousClock.now.advanced(by: .seconds(3))
    while !condition() {
        guard ContinuousClock.now < deadline else { throw ContractError.invalid("The native screen never settled") }
        try await Task.sleep(for: .milliseconds(5))
    }
}
@MainActor func automationModel(_ transport: NativeAutomationTransport) async throws -> RemoteModel {
    let model = try RemoteModel(client: automationClient(transport))
    model.show(.scenes); model.resume()
    try await automationEventually { model.house?.isFresh == true && !model.panelLoading && !model.scenes.isEmpty }
    return model
}

@Test func nativeScenesDecodeWholeHouseDefinitionsAndEncodeNamesAsOnePathElement() async throws {
    let transport = NativeAutomationTransport(), client = try automationClient(transport)
    let scenes = try await client.scenes()
    #expect(scenes == [automationScene()])
    #expect(try await client.scene(named: scenes[0].name) == scenes[0])
    let url = try #require(await transport.requests.last?.url)
    #expect(url.absoluteString.contains("Dinner%20%2F%20caf"))
    #expect(url.host == "daemon.example.test")
    #expect(await transport.writes().isEmpty)
}

@Test func nativeScenesRejectDuplicateNamesInvalidLevelsAndUnknownSources() async throws {
    let transport = NativeAutomationTransport(), client = try automationClient(transport)
    await transport.replaceList([automationScene(), automationScene()])
    await #expect(throws: ContractError.self) { try await client.scenes() }
    await transport.replaceList([automationScene(level: 101)])
    await #expect(throws: ContractError.self) { try await client.scenes() }
    let unsupported = Scene(name: "Unknown", saved: 1,
                            groups: [.init(coordinator: "Workshop", members: [],
                                           source: .init(kind: "new-kind", name: nil, uri: nil, mood: nil), playing: nil)],
                            volumes: [:], mutes: [:])
    #expect(throws: ContractError.self) { try unsupported.validated() }
    #expect(await transport.writes().isEmpty)
}

@Test func nativeSceneSingleReadMustNameTheRequestedDefinition() async throws {
    let transport = NativeAutomationTransport()
    await transport.wrongName()
    await #expect(throws: ContractError.self) { try await automationClient(transport).scene(named: automationScene().name) }
}

@Test func nativeSceneReviewAppliesExactlyTheStoredDefinitionAcrossHouseholdsOnce() async throws {
    let transport = NativeAutomationTransport(), client = try automationClient(transport)
    let session = CommandReviewSession(client: client)
    let review = try await session.prepare(.scene(automationScene()), in: automationZone())
    #expect(await transport.writes().isEmpty)
    let receipt = try await session.execute(review)
    #expect(receipt.title == "Scene applied" && !receipt.partial)
    #expect(receipt.detail.contains("Workshop@TEST-B"))
    let posts = await transport.writes()
    #expect(posts.count == 1)
    #expect(posts[0].url!.absoluteString.contains("%2F"))
    #expect(try JSONDecoder().decode([String: JSONValue].self, from: posts[0].httpBody!) == [:])
    // Selected TEST-A is an initiating anchor, never an extra scope filter.
    #expect(posts[0].url?.query == nil)
    await #expect(throws: ContractError.self) { try await session.execute(review) }
    #expect(await transport.writes().count == 1)
}

@Test func nativeSceneChangedDefinitionOrHouseStateRequiresANewReview() async throws {
    for changeScene in [true, false] {
        let transport = NativeAutomationTransport(), session = try CommandReviewSession(client: automationClient(transport))
        let review = try await session.prepare(.scene(automationScene()), in: automationZone())
        if changeScene { await transport.replaceScene(automationScene(level: 30, saved: 2)) }
        else { await transport.changeLevel() }
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes().isEmpty)
    }
}

@Test func nativeScenePartialReceiptPreservesAllFailuresSkippedRoomsAndPolicyNotes() async throws {
    let transport = NativeAutomationTransport(), session = try CommandReviewSession(client: automationClient(transport))
    let review = try await session.prepare(.scene(automationScene()), in: automationZone())
    await transport.partialResult()
    let receipt = try await session.execute(review)
    #expect(receipt.partial)
    for text in ["Grouped Workshop@TEST-B", "Set level", "Speaker unavailable", "Missing room", "Capped at 25"] {
        #expect(receipt.detail.contains(text))
    }
    #expect(await transport.writes().count == 1)
}

@Test func nativeSceneLostAndMalformedRepliesRemainSingleShot() async throws {
    for malformed in [true, false] {
        let transport = NativeAutomationTransport(), session = try CommandReviewSession(client: automationClient(transport))
        let review = try await session.prepare(.scene(automationScene()), in: automationZone())
        if malformed { await transport.breakWrite() } else { await transport.loseWrite() }
        await #expect(throws: ClientError.self) { try await session.execute(review) }
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes().count == 1)
    }
}

@Test @MainActor func nativeScenesScreenListsReviewsAndShowsTheHouseScopeOfItsResult() async throws {
    let transport = NativeAutomationTransport(), model = try await automationModel(transport)
    defer { model.stop() }
    model.prepare(.scene(try #require(model.scenes.first)))
    try await automationEventually { model.review != nil }
    let id = try #require(model.review?.id)
    model.confirm(id); model.confirm(id)
    try await automationEventually { model.actionPhase == .idle && model.result != nil }
    #expect(model.result?.scene == automationScene().name)
    #expect(model.result?.kind == .confirmed)
    #expect(await transport.writes().count == 1)
}

@Test @MainActor func nativeScenesCancelledListCannotRepopulateADifferentPanel() async throws {
    let transport = NativeAutomationTransport(), model = try await automationModel(transport)
    defer { model.stop() }
    await transport.hold("/scenes")
    model.refreshPanel()
    try await waitForAutomationRead(transport)
    model.show(.favorites)
    await transport.release()
    try await automationEventually { !model.panelLoading }
    #expect(model.scenes.isEmpty && model.panel == .favorites)
    #expect(await transport.writes().isEmpty)
}

@Test @MainActor func nativeScenesReadFailureRemovesStaleDefinitionsWithoutDisablingPlayback() async throws {
    let transport = NativeAutomationTransport(), model = try await automationModel(transport)
    defer { model.stop() }
    await transport.fail("/scenes")
    model.refreshPanel()
    try await automationEventually { model.panelProblem != nil && !model.panelLoading }
    #expect(model.scenes.isEmpty && model.canPrepare)
    model.prepare(.pause)
    try await automationEventually { model.review != nil }
    model.confirm(try #require(model.review?.id))
    try await automationEventually { model.result?.kind == .confirmed }
    #expect(await transport.writes().count == 1)
}

@Test @MainActor func nativeSceneSuspendDuringFinalDefinitionReadPreventsDispatch() async throws {
    let transport = NativeAutomationTransport(), model = try await automationModel(transport)
    defer { model.stop() }
    model.prepare(.scene(automationScene()))
    try await automationEventually { model.review != nil }
    await transport.hold("/scenes/" + automationScene().name)
    model.confirm(try #require(model.review?.id))
    try await waitForAutomationRead(transport)
    model.suspend()
    await transport.release()
    try await automationEventually { model.actionPhase == .idle }
    #expect(model.result?.kind == .notSent)
    #expect(model.result?.scene == automationScene().name)
    #expect(await transport.writes().isEmpty)
}

@Test @MainActor func nativeSceneLatePartialOutcomeSurvivesSelectionChangeAndRefresh() async throws {
    let transport = NativeAutomationTransport(), model = try await automationModel(transport)
    defer { model.stop() }
    await transport.partialResult()
    model.prepare(.scene(automationScene()))
    try await automationEventually { model.review != nil }
    await transport.hold("/scenes/" + automationScene().name + "/apply", method: "POST")
    model.confirm(try #require(model.review?.id))
    try await waitForAutomationRead(transport)
    model.select(automationZone(house: "TEST-B").id)
    await transport.release()
    try await automationEventually { model.actionPhase == .idle && model.result != nil }
    #expect(model.result?.scene == automationScene().name && model.result?.kind == .partial)
    model.refresh()
    try await Task.sleep(for: .milliseconds(30))
    #expect(model.result?.detail.contains("Speaker unavailable") == true)
    #expect(await transport.writes().count == 1)
}
