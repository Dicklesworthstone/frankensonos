import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

private func tasteData(energy: Int? = nil) throws -> Data {
    try JSONSerialization.data(withJSONObject: [
        "energy": energy.map { $0 as Any } ?? NSNull(), "explicit": false,
        "favor": ["genres": [], "artists": [], "eras": [], "moods": []],
        "avoid": ["genres": [], "artists": [], "eras": [], "moods": []],
        "pin": ["artists": [], "albums": [], "tracks": []],
        "ban": ["artists": [], "albums": [], "tracks": []]
    ] as [String: Any])
}
private actor TasteTransport: DaemonTransport {
    var calls: [URLRequest] = []
    var preferences: Data
    var writeReply: HTTPReply?
    var loseReply = false
    var holdRead = 0
    var holdWrite = false
    var reads = 0
    private var waiting: CheckedContinuation<Void, Never>?
    init() throws { preferences = try tasteData() }
    func configure(energy: Int? = nil, holdRead: Int = 0, holdWrite: Bool = false, loseReply: Bool = false, writeReply: HTTPReply? = nil) throws {
        preferences = try tasteData(energy: energy); self.holdRead = holdRead
        self.holdWrite = holdWrite; self.loseReply = loseReply; self.writeReply = writeReply
    }
    func release() { waiting?.resume(); waiting = nil }
    func send(_ request: URLRequest) async throws -> HTTPReply {
        calls.append(request)
        guard request.url?.path == "/dj/preferences" else {
            return HTTPReply(data: Data("{\"code\":\"NOT_IMPLEMENTED\",\"detail\":\"No speaker fixture\"}".utf8), status: 503)
        }
        if request.httpMethod != "POST" {
            reads += 1; let observed = preferences
            if reads == holdRead { await withCheckedContinuation { waiting = $0 } }
            return HTTPReply(data: observed, status: 200)
        }
        if holdWrite { await withCheckedContinuation { waiting = $0 } }
        if loseReply { throw URLError(.networkConnectionLost) }
        if let writeReply { return writeReply }
        let body = try #require(JSONSerialization.jsonObject(with: #require(request.httpBody)) as? [String: Any])
        if body["key"] as? String == "energy" {
            preferences = try tasteData(energy: body["unset"] as? Bool == true ? nil : Int(body["value"] as? String ?? ""))
        }
        let object = try JSONSerialization.jsonObject(with: preferences)
        return HTTPReply(data: try JSONSerialization.data(withJSONObject: ["done": "Standing taste saved", "changed": true, "preferences": object]), status: 200)
    }
    nonisolated func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> {
        AsyncThrowingStream { $0.finish() }
    }
    var writes: [URLRequest] { calls.filter { $0.httpMethod == "POST" } }
}
private func tasteClient(_ transport: TasteTransport) throws -> DaemonClient {
    DaemonClient(endpoint: try DaemonEndpoint("https://daemon.example"), transport: transport)
}
private func tasteCommand() throws -> DJPreferenceCommand {
    try DJPreferenceCommand(change: DJPreferenceChange(key: .energy, value: "30"),
                            before: DaemonClient.decode(DJPreferences.self, from: tasteData()))
}
@MainActor private func tasteWait(_ condition: () async -> Bool) async throws {
    for _ in 0..<300 { if await condition() { return }; try await Task.sleep(for: .milliseconds(2)) }
    Issue.record("Expected preference operation did not finish")
}

@Suite("Standing DJ preference command integration") struct DJPreferencesCommandTests {
    @Test func readIsDaemonScopedAndNeverRequestsRoomsOrPlayback() async throws {
        let transport = try TasteTransport(), client = try tasteClient(transport)
        _ = try await client.djPreferences()
        let request = try #require(await transport.calls.first)
        #expect(request.httpMethod == "GET" && request.url?.path == "/dj/preferences" && request.url?.query == nil)
        #expect(request.value(forHTTPHeaderField: "Accept") == "application/json")
        #expect(request.value(forHTTPHeaderField: "Authorization") == nil)
        #expect(request.httpBody == nil)
    }
    @Test func preparationAndConfirmationReadThenSendOneExactPreference() async throws {
        let transport = try TasteTransport(), session = CommandReviewSession(client: try tasteClient(transport))
        let review = try await session.prepare(tasteCommand())
        #expect(await transport.writes.isEmpty)
        #expect(try await session.execute(review).preferences.energy == 30)
        let calls = await transport.calls
        #expect(calls.map { $0.httpMethod ?? "GET" } == ["GET", "GET", "POST"])
        #expect(calls.allSatisfy { $0.url?.path == "/dj/preferences" })
        let body = try #require(JSONSerialization.jsonObject(with: #require(calls.last?.httpBody)) as? [String: Any])
        #expect(Set(body.keys) == ["key", "value", "unset"])
        #expect(body["key"] as? String == "energy" && body["value"] as? String == "30")
        await #expect(throws: (any Error).self) { try await session.execute(review) }
        #expect(await transport.writes.count == 1)
    }
    @Test func changedBaselinePreventsBothPreparationAndConfirmation() async throws {
        let transport = try TasteTransport(), session = CommandReviewSession(client: try tasteClient(transport))
        try await transport.configure(energy: 40)
        await #expect(throws: (any Error).self) { try await session.prepare(tasteCommand()) }
        try await transport.configure()
        let review = try await session.prepare(tasteCommand())
        try await transport.configure(energy: 40)
        await #expect(throws: (any Error).self) { try await session.execute(review) }
        #expect(await transport.writes.isEmpty)
    }
    @Test func supersededAndForeignReviewsCannotSave() async throws {
        let transport = try TasteTransport(), client = try tasteClient(transport)
        let session = CommandReviewSession(client: client), other = CommandReviewSession(client: client)
        let first = try await session.prepare(tasteCommand()), second = try await session.prepare(tasteCommand())
        await #expect(throws: (any Error).self) { try await session.execute(first) }
        await #expect(throws: (any Error).self) { try await other.execute(second) }
        #expect(await transport.writes.isEmpty)
        _ = try await session.execute(second)
        #expect(await transport.writes.count == 1)
    }
    @Test func cancellationDuringFinalReadCannotDispatch() async throws {
        let transport = try TasteTransport(), session = CommandReviewSession(client: try tasteClient(transport))
        let review = try await session.prepare(tasteCommand())
        try await transport.configure(holdRead: 2)
        let execution = Task { try await session.execute(review) }
        try await tasteWait { await transport.reads == 2 }
        await session.cancel(); await transport.release()
        await #expect(throws: (any Error).self) { try await execution.value }
        #expect(await transport.writes.isEmpty)
    }
    @Test func preferenceWriteOccupiesTheExistingPlaybackAndUndoGate() async throws {
        let transport = try TasteTransport(), session = CommandReviewSession(client: try tasteClient(transport))
        let review = try await session.prepare(tasteCommand())
        try await transport.configure(holdWrite: true)
        let saving = Task { try await session.execute(review) }
        try await tasteWait { await transport.writes.count == 1 }
        let zone = try DaemonClient.decode(Zone.self, from: Data("{\"household\":\"Fixture\",\"coordinator_room\":\"Room\",\"members\":[\"Room\"],\"transport_state\":\"paused\"}".utf8))
        await #expect(throws: (any Error).self) { try await session.prepare(.pause, in: zone) }
        await #expect(throws: (any Error).self) { try await session.prepareUndo() }
        await #expect(throws: (any Error).self) { try await session.prepare(tasteCommand()) }
        #expect(await transport.calls.count == 3)
        await transport.release(); _ = try await saving.value
    }
    @Test func lostMalformedAndServerErrorRepliesRemainUncertainAndSingleShot() async throws {
        for failure in 0..<4 {
            let transport = try TasteTransport(), session = CommandReviewSession(client: try tasteClient(transport))
            let review = try await session.prepare(tasteCommand())
            let response: HTTPReply?
            switch failure {
            case 1: response = HTTPReply(data: Data("null".utf8), status: 200)
            case 2: response = HTTPReply(data: Data("{\"done\":\"Saved\",\"changed\":true}".utf8), status: 200)
            case 3: response = HTTPReply(data: Data("{\"code\":\"UNAVAILABLE\",\"detail\":\"Unavailable\"}".utf8), status: 503)
            default: response = nil
            }
            try await transport.configure(loseReply: failure == 0, writeReply: response)
            do { _ = try await session.execute(review); Issue.record("Expected an uncertain save") }
            catch { guard case ClientError.uncertain = error else { Issue.record("Wrong error: \(error)"); continue } }
            await #expect(throws: (any Error).self) { try await session.execute(review) }
            #expect(await transport.writes.count == 1)
        }
    }
}

@Suite("Native standing taste workflow") @MainActor struct DJPreferencesWorkflowTests {
    @Test func savesWithoutAnySelectedRoomThroughTheProductionSession() async throws {
        let transport = try TasteTransport(), client = try tasteClient(transport)
        let workflow = DJPreferencesWorkflow(client: client, commands: CommandReviewSession(client: client))
        workflow.resume(); try await tasteWait { workflow.canPrepare }
        workflow.prepare(try DJPreferenceChange(key: .energy, value: "30"), from: #require(workflow.editor.observation))
        try await tasteWait { workflow.review != nil }
        #expect(await transport.writes.isEmpty)
        workflow.confirm(try #require(workflow.review).id)
        try await tasteWait { workflow.result != nil && !workflow.isEngaged }
        #expect(workflow.result?.kind == .confirmed)
        #expect(await transport.writes.count == 1)
        #expect(await transport.calls.allSatisfy { $0.url?.path == "/dj/preferences" })
        workflow.stop()
    }
    @Test func anotherNativeReviewPreventsPreferencePreparation() async throws {
        let transport = try TasteTransport(), client = try tasteClient(transport)
        let workflow = DJPreferencesWorkflow(client: client, commands: CommandReviewSession(client: client))
        workflow.resume(); try await tasteWait { workflow.canPrepare }
        workflow.canBegin = { false }
        workflow.prepare(try DJPreferenceChange(key: .energy, value: "30"), from: #require(workflow.editor.observation))
        #expect(!workflow.isEngaged && workflow.review == nil && !workflow.canPrepare)
        #expect(await transport.reads == 1); workflow.stop()
    }
    @Test func cancellationRetainsOwnershipUntilTheReadAndCancellationDrain() async throws {
        let transport = try TasteTransport(), client = try tasteClient(transport)
        let workflow = DJPreferencesWorkflow(client: client, commands: CommandReviewSession(client: client))
        workflow.resume(); try await tasteWait { workflow.canPrepare }
        try await transport.configure(holdRead: 2)
        workflow.prepare(try DJPreferenceChange(key: .energy, value: "30"), from: #require(workflow.editor.observation))
        try await tasteWait { await transport.reads == 2 }
        workflow.cancel(); #expect(workflow.isEngaged)
        workflow.refresh(); #expect(await transport.reads == 2)
        await transport.release(); try await tasteWait { !workflow.isEngaged }
        #expect(workflow.review == nil)
        #expect(await transport.writes.isEmpty)
        workflow.stop()
    }
    @Test func lateUncertaintySurvivesHideShowAndOnlyExplicitRefreshClearsIt() async throws {
        let transport = try TasteTransport(), client = try tasteClient(transport)
        let workflow = DJPreferencesWorkflow(client: client, commands: CommandReviewSession(client: client))
        workflow.resume(); try await tasteWait { workflow.canPrepare }
        workflow.prepare(try DJPreferenceChange(key: .energy, value: "30"), from: #require(workflow.editor.observation))
        try await tasteWait { workflow.review != nil }
        try await transport.configure(holdWrite: true, loseReply: true)
        workflow.confirm(try #require(workflow.review).id)
        try await tasteWait { await transport.writes.count == 1 }
        workflow.suspend(); await transport.release()
        try await tasteWait { workflow.result != nil && !workflow.isEngaged }
        workflow.resume(); try await tasteWait { workflow.editor.canEdit }
        #expect(workflow.needsRefresh && !workflow.canPrepare && workflow.result?.kind == .uncertain)
        workflow.refresh(); try await tasteWait { workflow.canPrepare }
        #expect(!workflow.needsRefresh && workflow.result?.kind == .uncertain)
        #expect(await transport.writes.count == 1); workflow.stop()
    }
    @Test func stoppedWorkflowCannotSaveOrReadAgain() async throws {
        let transport = try TasteTransport(), client = try tasteClient(transport)
        let workflow = DJPreferencesWorkflow(client: client, commands: CommandReviewSession(client: client))
        workflow.resume(); try await tasteWait { workflow.canPrepare }
        let observed = try #require(workflow.editor.observation)
        workflow.stop(); workflow.resume(); workflow.refresh()
        workflow.prepare(try DJPreferenceChange(key: .energy, value: "30"), from: observed)
        #expect(!workflow.active && !workflow.isEngaged && !workflow.canPrepare)
        #expect(await transport.calls.count == 1)
    }
}
