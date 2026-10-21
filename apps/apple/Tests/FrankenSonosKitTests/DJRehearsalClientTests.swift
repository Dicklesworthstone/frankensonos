import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

private func previewZone(_ household: String = "S2", members: [String] = ["Studio", "Hall"]) -> Zone {
    Zone(household: household, coordinatorRoom: "Studio", members: members, transportState: "stopped")
}
private let previewJSON = #"{"zone":"Studio","from":"2026-10-20T23:00:00-04:00","seed":1,"works":[{"at":"2026-10-20T23:00:00-04:00","work":{"composer":"Example Artist","title":"Example Work","movements":3,"minutes":12,"reason":{"summary":"Balances the set","factors":[{"factor":"energy_fit","weight":1200}],"relaxed":["genre"]}},"genre":"classical","energy":35,"energy_target":40,"explicit":false}],"summary":{"works":1,"minutes":12,"distinct_artists":1,"explicit":0,"hours":[{"from":"2026-10-20T23:00:00-04:00","works":1,"artist_entropy":0,"genre_entropy":0,"energy":35,"energy_target":40}]}}"#

private actor PreviewTransport: DaemonTransport {
    var requests: [URLRequest] = []
    var groups = [previewZone()]
    var reply = previewJSON
    var status = 200
    var failure: ClientError?
    var heldPath: String?
    var waiter: CheckedContinuation<Void, Never>?
    func configure(groups: [Zone]? = nil, reply: String = previewJSON, status: Int = 200,
                   failure: ClientError? = nil, hold: String? = nil) {
        if let groups { self.groups = groups }
        self.reply = reply; self.status = status; self.failure = failure; heldPath = hold
    }
    func release() { heldPath = nil; let current = waiter; waiter = nil; current?.resume() }
    func send(_ request: URLRequest) async throws -> HTTPReply {
        requests.append(request)
        if request.url?.path == heldPath { await withCheckedContinuation { waiter = $0 } }
        if request.url?.path == "/zones" {
            let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
            return HTTPReply(data: try encoder.encode(groups), status: 200)
        }
        #expect(request.url?.path == "/dj/rehearse")
        if let failure { throw failure }
        return HTTPReply(data: Data(reply.utf8), status: status)
    }
    nonisolated func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> {
        AsyncThrowingStream { $0.finish() }
    }
}
private func previewClient(_ transport: PreviewTransport) throws -> DaemonClient {
    DaemonClient(endpoint: try DaemonEndpoint("https://example.invalid"), transport: transport)
}
private func previewWait(_ condition: @Sendable () async -> Bool) async throws {
    let clock = ContinuousClock(), end = clock.now + .seconds(3)
    while clock.now < end {
        if await condition() { return }
        try await Task.sleep(for: .milliseconds(2))
    }
    throw ContractError.invalid("Preview fixture did not reach the expected state")
}

@Test func rehearsalClientUsesOneQualifiedReadOnlyPostWithExactIntegerSeed() async throws {
    let transport = PreviewTransport(), client = try previewClient(transport)
    var draft = DJRehearsalDraft()
    draft.seed = String(UInt64.max); draft.length = .hours; draft.hours = 2
    draft.start = .houseTime; draft.houseTime = "23:00"; draft.mood = "calm"
    draft.filters[.includeArtists] = "Example Artist\nAnother, Artist"
    let json = previewJSON.replacingOccurrences(of: #""seed":1"#, with: #""seed":18446744073709551615"#)
    await transport.configure(reply: json)
    let request = try draft.request(for: previewZone())
    let report = try await client.rehearse(request)
    let requests = await transport.requests
    #expect(requests.count == 2)
    #expect(requests[0].url?.path == "/zones")
    #expect(requests[0].httpMethod == "GET")
    let sent = try #require(requests.last)
    #expect(sent.url?.absoluteString == "https://example.invalid/dj/rehearse")
    #expect(sent.httpMethod == "POST" && sent.timeoutInterval == 120)
    #expect(sent.value(forHTTPHeaderField: "Accept") == "application/json")
    #expect(sent.value(forHTTPHeaderField: "Content-Type") == "application/json")
    struct Body: Decodable {
        let zone: String; let seed: UInt64; let hours: Int; let at: String; let mood: String
        let constraints: [String: JSONValue]
    }
    let data = try #require(sent.httpBody), body = try JSONDecoder().decode(Body.self, from: data)
    #expect(body.zone == "Studio@S2" && body.seed == UInt64.max && report.seed == UInt64.max)
    #expect(body.hours == 2 && body.at == "23:00" && body.mood == "calm")
    #expect(body.constraints["include_artists"] == .array([.string("Example Artist"), .string("Another, Artist")]))
    let fields = try #require(JSONSerialization.jsonObject(with: data) as? [String: Any])
    #expect(Set(fields.keys) == ["zone", "seed", "hours", "at", "mood", "constraints"])
}

@Test func rehearsalClientDefaultsInheritSteeringWithoutSendingAnOverride() async throws {
    let transport = PreviewTransport()
    _ = try await previewClient(transport).rehearse(DJRehearsalDraft().request(for: previewZone()))
    let data = try #require(await transport.requests.last?.httpBody)
    let fields = try JSONDecoder().decode([String: JSONValue].self, from: data)
    #expect(fields == ["zone": .string("Studio@S2"), "works": .integer(12), "seed": .integer(1)])
}

@Test func rehearsalClientRefusesChangedMissingAndCrossHouseholdGroupsBeforePost() async throws {
    for groups in [[], [previewZone("S1")], [previewZone(members: ["Studio"])]] as [[Zone]] {
        let transport = PreviewTransport()
        await transport.configure(groups: groups)
        await #expect(throws: ContractError.self) {
            try await previewClient(transport).rehearse(DJRehearsalDraft().request(for: previewZone()))
        }
        #expect(await transport.requests.count == 1)
    }
}

@Test func rehearsalClientAllowsHarmlessMemberOrderAndPlaybackProgress() async throws {
    let transport = PreviewTransport()
    let current = Zone(household: "S2", coordinatorRoom: "Studio", members: ["Hall", "Studio"], transportState: "playing")
    await transport.configure(groups: [current])
    let value = try await previewClient(transport).rehearse(DJRehearsalDraft().request(for: previewZone()))
    #expect(value.works.count == 1 && value.works[0].work.movements == 3)
}

@Test func rehearsalClientKeepsAPIProblemsReadOnlyAndNeverRetries() async throws {
    let transport = PreviewTransport()
    let problem = #"{"code":"NOT_READY","detail":"Library not synced","hint":"Sync the library"}"#
    await transport.configure(reply: problem, status: 503)
    do {
        _ = try await previewClient(transport).rehearse(DJRehearsalDraft().request(for: previewZone()))
        Issue.record("Expected a read error")
    } catch let error as APIProblem {
        #expect(error.code == "NOT_READY" && error.hint == "Sync the library")
    }
    #expect(await transport.requests.count == 2)
}

@Test func rehearsalClientDoesNotDescribeFailedPreviewAsAnUncertainPlaybackWrite() async throws {
    let transport = PreviewTransport()
    await transport.configure(failure: .http(503, "preview unavailable"))
    await #expect(throws: ClientError.http(503, "preview unavailable")) {
        try await previewClient(transport).rehearse(DJRehearsalDraft().request(for: previewZone()))
    }
    #expect(await transport.requests.count == 2)
}

@Test func rehearsalClientRejectsMismatchedOrMalformedRepliesWithoutReplaying() async throws {
    for json in ["{}", previewJSON.replacingOccurrences(of: #""seed":1"#, with: #""seed":2"#),
                 previewJSON.replacingOccurrences(of: #""zone":"Studio""#, with: #""zone":"Hall""#),
                 previewJSON.replacingOccurrences(of: #""artist_entropy":0"#, with: #""artist_entropy":1"#)] {
        let transport = PreviewTransport()
        await transport.configure(reply: json)
        await #expect(throws: (any Error).self) {
            try await previewClient(transport).rehearse(DJRehearsalDraft().request(for: previewZone()))
        }
        #expect(await transport.requests.count == 2)
    }
}

@Test func rehearsalCancellationDuringTopologyStopsBeforeGenerating() async throws {
    let transport = PreviewTransport(), client = try previewClient(transport)
    await transport.configure(hold: "/zones")
    let request = try DJRehearsalDraft().request(for: previewZone())
    let task = Task { try await client.rehearse(request) }
    try await previewWait { await transport.waiter != nil }
    task.cancel(); await transport.release()
    await #expect(throws: CancellationError.self) { try await task.value }
    #expect(await transport.requests.count == 1)
}

@Test func rehearsalCancellationDuringGenerationDiscardsLateReplyWithoutAnotherPost() async throws {
    let transport = PreviewTransport(), client = try previewClient(transport)
    await transport.configure(hold: "/dj/rehearse")
    let request = try DJRehearsalDraft().request(for: previewZone())
    let task = Task { try await client.rehearse(request) }
    try await previewWait { await transport.waiter != nil }
    task.cancel(); await transport.release()
    await #expect(throws: CancellationError.self) { try await task.value }
    #expect(await transport.requests.count == 2)
}

@Test func rehearsalReportRetainsDaemonTimeWholeWorksAndRelaxationReasons() throws {
    let request = try DJRehearsalDraft().request(for: previewZone())
    let report = try DaemonClient.decode(DJRehearsal.self, from: Data(previewJSON.utf8)).validated(for: request)
    #expect(DJRehearsal.clock(report.from) == "23:00")
    #expect(DJRehearsal.day(report.from) == "2026-10-20")
    #expect(DJRehearsal.offset(report.from) == "UTC-04:00")
    #expect(report.relaxedWorkCount == 1)
    #expect(report.works[0].work.reason.factors?.first?.weight == 1200)
    let text = report.text(for: request)
    #expect(text.contains("Preview only") && text.contains("3 movement(s)"))
    #expect(text.contains("Relaxed filters: genre") && text.contains("may choose differently"))
}

private actor PreviewReader {
    var calls = 0
    var continuation: CheckedContinuation<DJRehearsal, any Error>?
    var captured: DJRehearsalRequest?
    func read(_ request: DJRehearsalRequest) async throws -> DJRehearsal {
        calls += 1; captured = request
        return try await withCheckedThrowingContinuation { continuation = $0 }
    }
    func finish(failing: Bool = false) throws {
        let current = continuation; continuation = nil
        if failing { current?.resume(throwing: ClientError.http(503, "preview failed")); return }
        let seed = captured?.seed ?? 1
        let json = previewJSON.replacingOccurrences(of: #""seed":1"#, with: "\"seed\":\(seed)")
        current?.resume(returning: try DaemonClient.decode(DJRehearsal.self, from: Data(json.utf8)))
    }
}

@Test @MainActor func rehearsalModelGeneratesOnlyOnExplicitRequestAndBlocksOverlappingWork() async throws {
    let reader = PreviewReader(), model = DJRehearsalModel(read: { try await reader.read($0) })
    model.activate(in: previewZone())
    var draft = model.draft; draft.seed = "2"; model.update(draft)
    #expect(await reader.calls == 0)
    model.generate(); model.generate()
    try await previewWait { await reader.continuation != nil }
    #expect(await reader.calls == 1)
    #expect(model.loading && !model.canGenerate)
    try await reader.finish()
    try await previewWait { await MainActor.run { !model.loading } }
    #expect(model.observation?.report.seed == 2 && model.matchesChoices)
    model.stop()
}

@Test @MainActor func rehearsalModelEditsCancelOldWorkAndDoNotQueueAnAutomaticReplacement() async throws {
    let reader = PreviewReader(), model = DJRehearsalModel(read: { try await reader.read($0) })
    model.activate(in: previewZone()); model.generate()
    try await previewWait { await reader.continuation != nil }
    var draft = model.draft; draft.seed = "3"; model.update(draft)
    model.generate()
    #expect(!model.canGenerate && model.loading)
    try await reader.finish()
    try await previewWait { await MainActor.run { !model.loading } }
    #expect(model.observation == nil && !model.matchesChoices)
    #expect(await reader.calls == 1)
    model.generate()
    try await previewWait { await reader.continuation != nil }
    try await reader.finish()
    try await previewWait { await MainActor.run { !model.loading } }
    #expect(model.observation?.report.seed == 3)
    #expect(await reader.calls == 2)
    model.stop()
}

@Test @MainActor func rehearsalModelNeverRetargetsLateResultsAcrossHouseholds() async throws {
    let reader = PreviewReader(), model = DJRehearsalModel(read: { try await reader.read($0) })
    model.activate(in: previewZone()); model.generate()
    try await previewWait { await reader.continuation != nil }
    model.activate(in: previewZone("S1"))
    try await reader.finish()
    try await previewWait { await MainActor.run { !model.loading } }
    #expect(model.observation == nil && !model.matchesChoices)
    #expect(model.zone?.household == "S1")
    #expect(await reader.calls == 1)
    model.stop()
}

@Test @MainActor func rehearsalModelSuspensionRetainsOldPreviewButNeverCallsItCurrent() async throws {
    let reader = PreviewReader(), model = DJRehearsalModel(read: { try await reader.read($0) })
    model.activate(in: previewZone()); model.generate()
    try await previewWait { await reader.continuation != nil }; try await reader.finish()
    try await previewWait { await MainActor.run { !model.loading } }
    model.suspend()
    #expect(!model.canGenerate && !model.matchesChoices && model.observation != nil)
    model.activate(in: previewZone())
    #expect(model.canGenerate && !model.matchesChoices)
    #expect(await reader.calls == 1)
    model.stop(); model.activate(in: previewZone()); model.generate()
    #expect(!model.canGenerate)
    #expect(await reader.calls == 1)
}

@Test @MainActor func rehearsalModelRejectsLateSuccessAndErrorAfterCancellation() async throws {
    for failing in [false, true] {
        let reader = PreviewReader(), model = DJRehearsalModel(read: { try await reader.read($0) })
        model.activate(in: previewZone()); model.generate()
        try await previewWait { await reader.continuation != nil }
        model.cancel()
        let message = model.problem
        try await reader.finish(failing: failing)
        try await previewWait { await MainActor.run { !model.loading } }
        #expect(model.observation == nil && model.problem == message && !model.matchesChoices)
        model.stop()
    }
}

@Test @MainActor func rehearsalModelAllowsHarmlessProgressWithoutRegenerating() async throws {
    let reader = PreviewReader(), model = DJRehearsalModel(read: { try await reader.read($0) })
    model.activate(in: previewZone()); model.generate()
    try await previewWait { await reader.continuation != nil }; try await reader.finish()
    try await previewWait { await MainActor.run { !model.loading } }
    model.activate(in: Zone(household: "S2", coordinatorRoom: "Studio", members: ["Hall", "Studio"], transportState: "playing"))
    #expect(model.matchesChoices && model.observation != nil)
    #expect(await reader.calls == 1)
    model.stop()
}

@Test @MainActor func rehearsalModelDoesNotDispatchAQueuedTaskAfterImmediateCancel() async throws {
    let reader = PreviewReader(), model = DJRehearsalModel(read: { try await reader.read($0) })
    model.activate(in: previewZone()); model.generate(); model.cancel()
    // This actor has not yielded since generate, so the read task has not run.
    for _ in 0..<20 { await Task.yield() }
    let calls = await reader.calls
    // Drain a broken implementation before asserting, avoiding a leaked waiter.
    if calls > 0 { try await reader.finish() }
    try await previewWait { await MainActor.run { !model.loading } }
    #expect(calls == 0)
    model.stop()
}
