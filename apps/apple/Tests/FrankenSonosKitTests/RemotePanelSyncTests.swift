import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

private actor CatalogSyncTransport: DaemonTransport {
    var name = "Original", saved: Int64 = 1
    var requests: [URLRequest] = []
    var streams: [AsyncThrowingStream<EventSignal, Error>.Continuation] = []
    var holding = false, blocked = false, denied = false
    var reading = 0, peak = 0
    private var waiter: CheckedContinuation<Void, Never>?

    func replace(_ name: String) { self.name = name; saved += 1 }
    func hold() { holding = true }
    func deny() { denied = true }
    func release() { blocked = false; waiter?.resume(); waiter = nil }
    func count(_ path: String) -> Int { requests.filter { $0.url?.path == path }.count }
    private func scene() -> [String: Any] {
        ["name": name, "saved": saved, "groups": [], "volumes": [:], "mutes": [:]]
    }
    func send(_ request: URLRequest) async throws -> HTTPReply {
        requests.append(request)
        guard request.httpMethod == nil || request.httpMethod == "GET" else {
            throw ContractError.invalid("An event must never send a mutation")
        }
        let path = request.url!.path
        let data: Data
        if path == "/zones" { data = Data("[]".utf8) }
        else if path == "/schedules" { data = Data("[]".utf8) }
        else if path == "/scenes" {
            reading += 1; peak = max(peak, reading)
            defer { reading -= 1 }
            if denied {
                emit(intent: "list_scenes", decision: "deny: fixture policy")
                throw APIProblem(code: "POLICY_DENIED", detail: "Scene read denied", hint: nil)
            }
            // Capture before blocking, like an obsolete response already in flight.
            data = try JSONSerialization.data(withJSONObject: [scene()])
            if holding {
                holding = false; blocked = true
                await withCheckedContinuation { waiter = $0 }
            }
        } else if path == "/scenes/\(name)" {
            data = try JSONSerialization.data(withJSONObject: scene())
        } else { throw ContractError.invalid("Unexpected catalog sync route: \(path)") }
        return HTTPReply(data: data, status: 200)
    }
    nonisolated func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> {
        let pair = AsyncThrowingStream<EventSignal, Error>.makeStream()
        Task { await register(pair.continuation) }
        return pair.stream
    }
    private func register(_ output: AsyncThrowingStream<EventSignal, Error>.Continuation) {
        streams.append(output); output.yield(.connected)
    }
    func emit(intent: String, decision: String = "allow", count: Int = 1) {
        let data = try! JSONSerialization.data(withJSONObject: ["intent": intent, "decision": decision])
        for _ in 0..<count {
            streams.last?.yield(.event(ServerEvent(id: nil, kind: "action.logged", data: String(decoding: data, as: UTF8.self))))
        }
    }
    func reset() { streams.last?.yield(.event(ServerEvent(id: nil, kind: "events.reset", data: "{}"))) }
    func disconnect() { streams.last?.finish(throwing: ClientError.unreadable) }
}

@MainActor private func catalogEventually(_ condition: () async -> Bool) async throws {
    let end = ContinuousClock.now.advanced(by: .seconds(5))
    while !(await condition()) {
        guard ContinuousClock.now < end else { throw ContractError.invalid("Native catalog did not settle") }
        try await Task.sleep(for: .milliseconds(5))
    }
}
@MainActor private func catalogScreen(_ transport: CatalogSyncTransport, panel: RemoteModel.Panel = .scenes) async throws -> RemoteModel {
    let client = DaemonClient(endpoint: try DaemonEndpoint("https://daemon.example.test"), transport: transport)
    let model = RemoteModel(client: client)
    model.resume(); model.show(panel)
    do {
        try await catalogEventually {
            model.house?.isFresh == true && (model.house?.panelRevision ?? 0) > 0 && !model.panelLoading &&
                (panel == .scenes ? model.scenesObservedAt != nil : model.schedulesObservedAt != nil)
        }
        return model
    } catch { model.stop(); throw error }
}

@Test @MainActor func nativeSceneEventsUpdateAnEmptyHouseWithoutDiscardingTheDraft() async throws {
    let transport = CatalogSyncTransport(), model = try await catalogScreen(transport)
    defer { model.stop() }
    #expect(model.selectedZone == nil)
    model.beginSceneDraft()
    var draft = model.sceneDraft; draft.name = "Unsent draft"
    model.updateSceneDraft(draft); model.updateSceneCaptureName("Capture name")
    let reads = await transport.count("/zones")
    await transport.replace("Updated elsewhere"); await transport.emit(intent: "save_scene: Updated elsewhere")
    try await catalogEventually { model.scenes.first?.name == "Updated elsewhere" && !model.panelLoading }
    #expect(model.sceneDraft.name == "Unsent draft" && model.sceneComposerOpen)
    #expect(model.sceneCaptureName == "Capture name")
    #expect(model.sceneMutationResult == nil && model.result == nil && model.actionPhase == .idle)
    #expect(await transport.count("/zones") == reads)
    #expect(await transport.requests.allSatisfy { ($0.httpMethod ?? "GET") == "GET" })
}

@Test @MainActor func nativeScheduleEventsRefreshTheListAndItsSceneChoices() async throws {
    let transport = CatalogSyncTransport(), model = try await catalogScreen(transport, panel: .schedules)
    defer { model.stop() }
    let before = await transport.count("/schedules")
    await transport.replace("New schedule scene"); await transport.emit(intent: "add_schedule: #7")
    try await catalogEventually { model.scheduleScenes.first?.name == "New schedule scene" && !model.panelLoading }
    #expect(await transport.count("/schedules") == before + 1)
    #expect(model.schedulesObservedAt != nil && model.selectedZone == nil)
    #expect(model.scheduleResult == nil && model.scheduleReview == nil)
}

@Test @MainActor func nativeCatalogRefreshDoesNotRetargetAnOpenSceneReview() async throws {
    let transport = CatalogSyncTransport(), model = try await catalogScreen(transport)
    defer { model.stop() }
    model.prepareSceneDelete(try #require(model.scenes.first))
    try await catalogEventually { model.sceneMutationReview != nil && model.actionPhase == .idle }
    let reviewed = try #require(model.sceneMutationReview)
    await transport.replace("Replacement"); await transport.emit(intent: "save_scene: Replacement")
    try await catalogEventually { model.scenes.first?.name == "Replacement" && !model.panelLoading }
    #expect(model.sceneMutationReview?.id == reviewed.id)
    #expect(model.sceneMutationReview?.command.name == "Original")
    #expect(model.sceneMutationResult == nil && !model.canManageScenes)
    #expect(await transport.requests.allSatisfy { ($0.httpMethod ?? "GET") == "GET" })
}

@Test @MainActor func nativeCatalogBurstsCoalesceBehindAnObsoleteRead() async throws {
    let transport = CatalogSyncTransport(), model = try await catalogScreen(transport)
    defer { model.stop() }
    let before = await transport.count("/scenes")
    let revision = try #require(model.house?.panelRevision)
    await transport.hold(); model.refreshPanel()
    try await catalogEventually { await transport.blocked }
    await transport.replace("Latest catalog"); await transport.emit(intent: "save_scene: Latest catalog", count: 100)
    try await catalogEventually { (model.house?.panelRevision ?? 0) > revision }
    #expect(await transport.count("/scenes") == before + 1)
    await transport.release()
    try await catalogEventually { !model.panelLoading && model.scenes.first?.name == "Latest catalog" }
    #expect(await transport.count("/scenes") == before + 2)
    #expect(await transport.peak == 1)
}

@Test @MainActor func nativeCatalogReadRefusalsDoNotRefreshThemselves() async throws {
    let transport = CatalogSyncTransport(), model = try await catalogScreen(transport)
    defer { model.stop() }
    await transport.deny(); model.refreshPanel()
    try await catalogEventually { model.panelProblem != nil && !model.panelLoading }
    let before = await transport.count("/scenes")
    try await Task.sleep(for: .milliseconds(600))
    #expect(await transport.count("/scenes") == before)
    #expect(model.scenesObservedAt == nil && !model.canManageScenes)
}

@Test @MainActor func nativeCatalogReconnectAndResetRecoverWithoutASelectedRoom() async throws {
    let transport = CatalogSyncTransport(), model = try await catalogScreen(transport)
    defer { model.stop() }
    await transport.replace("After reset"); await transport.reset()
    try await catalogEventually { model.scenes.first?.name == "After reset" && !model.panelLoading }
    await transport.disconnect(); await transport.replace("After reconnect")
    try await catalogEventually { model.scenes.first?.name == "After reconnect" && !model.panelLoading }
    #expect(model.selectedZone == nil && model.sceneMutationResult == nil)
    #expect(await transport.streams.count == 2)
    #expect(await transport.requests.allSatisfy { ($0.httpMethod ?? "GET") == "GET" })
}
