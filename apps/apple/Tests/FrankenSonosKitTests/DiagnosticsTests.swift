import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

// Same flattened, versioned shape as fsonos-core doctor::Report::to_json.
private func diagnosticBody(_ statuses: [String] = ["pass", "warn", "fail", "skip"]) -> [String: Any] {
    let counts = Dictionary(uniqueKeysWithValues: ["pass", "warn", "fail", "skip"].map { status in
        (status, statuses.filter { $0 == status }.count)
    })
    let checks: [[String: Any]] = statuses.enumerated().map { index, status in
        ["id": "setup.check\(index)", "title": "Check \(index)", "status": status,
         "summary": "Observed \(status)", "detail": NSNull(), "remedy": "Review host settings",
         "evidence": ["fixture": "private-evidence-only", "number": index], "duration_ms": index + 1]
    }
    return ["schema": 1, "exit_code": statuses.contains("fail") ? 7 : statuses.contains("warn") ? 6 : 0,
            "counts": counts, "checks": checks]
}
private func diagnosticData(_ body: [String: Any]) throws -> Data {
    try JSONSerialization.data(withJSONObject: body, options: [.sortedKeys])
}
private func diagnosticReport(_ body: [String: Any] = diagnosticBody()) throws -> DiagnosticsReport {
    try DaemonClient.decode(DiagnosticsReport.self, from: diagnosticData(body)).validated()
}

@Test func diagnosticReportReadsFlattenedChecksAndAllOutcomes() throws {
    let report = try diagnosticReport()
    #expect(report.schema == 1 && report.exitCode == 7)
    #expect(report.counts == .init(pass: 1, warn: 1, fail: 1, skip: 1))
    #expect(report.checks.map(\.status) == [.pass, .warn, .fail, .skip])
    #expect(report.checks[2].remedy == "Review host settings")
    #expect(report.checks[0].detail == nil)
    #expect(report.checks[0].evidenceText?.contains("private-evidence-only") == true)
    #expect(report.headline == "Setup problems found")
}

@Test func diagnosticReportAcceptsOmittedOptionalFieldsAndCompatibleAdditions() throws {
    var body = diagnosticBody(["pass"])
    var checks = try #require(body["checks"] as? [[String: Any]])
    for key in ["detail", "remedy", "evidence"] { checks[0].removeValue(forKey: key) }
    checks[0]["future_detail"] = "compatible extra field"
    body["checks"] = checks; body["future_total"] = 42
    let report = try diagnosticReport(body)
    #expect(report.checks[0].detail == nil && report.checks[0].remedy == nil)
    #expect(report.checks[0].evidenceText == nil)
}

@Test func diagnosticEmptyAndSkippedReportsNeverClaimHealthyPlayback() throws {
    let empty = try diagnosticReport(diagnosticBody([]))
    #expect(empty.headline == "No checks reported")
    let skipped = try diagnosticReport(diagnosticBody(["skip"]))
    #expect(skipped.exitCode == 0 && skipped.headline == "No checks ran")
    let partial = try diagnosticReport(diagnosticBody(["pass", "skip"]))
    #expect(partial.headline == "Completed checks passed")
    #expect(try diagnosticReport(diagnosticBody(["warn"])).headline == "Setup needs attention")
    #expect(try diagnosticReport(diagnosticBody(["pass"])).headline == "Reported checks passed")
}

@Test func diagnosticSchemaAndRequiredFieldsAreChecked() throws {
    for schema in [0, 2, -1, "1"] as [Any] {
        var body = diagnosticBody(); body["schema"] = schema
        #expect(throws: (any Error).self) { try diagnosticReport(body) }
    }
    for key in ["schema", "exit_code", "counts", "checks"] {
        var body = diagnosticBody(); body.removeValue(forKey: key)
        #expect(throws: (any Error).self) { try diagnosticReport(body) }
    }
}

@Test func diagnosticCountsAndExitCodeMustAgreeWithActualChecks() throws {
    for code in [0, 6, 8, 255] {
        var body = diagnosticBody(); body["exit_code"] = code
        #expect(throws: ContractError.self) { try diagnosticReport(body) }
    }
    for key in ["pass", "warn", "fail", "skip"] {
        var body = diagnosticBody(), counts = try #require(body["counts"] as? [String: Int])
        counts[key] = 0; body["counts"] = counts
        #expect(throws: ContractError.self) { try diagnosticReport(body) }
    }
    var body = diagnosticBody(["warn"]); body["exit_code"] = 7
    #expect(throws: ContractError.self) { try diagnosticReport(body) }
}

@Test func diagnosticInvalidChecksCannotBeDroppedToProduceSuccess() throws {
    let mutations: [(String, Any)] = [("status", "healthy"), ("status", NSNull()), ("duration_ms", -1),
        ("duration_ms", "5"), ("duration_ms", 0.5), ("id", ""), ("id", " \n"), ("summary", ""),
        ("title", ""), ("remedy", false), ("detail", ["unexpected"]), ("id", "bad\0name")]
    for (key, value) in mutations {
        var body = diagnosticBody(["pass"]), checks = try #require(body["checks"] as? [[String: Any]])
        checks[0][key] = value; body["checks"] = checks
        #expect(throws: (any Error).self) { try diagnosticReport(body) }
    }
}

@Test func diagnosticDuplicateAndExcessiveChecksAreRejected() throws {
    var body = diagnosticBody(["pass", "pass"]), checks = try #require(body["checks"] as? [[String: Any]])
    checks[1]["id"] = checks[0]["id"]; body["checks"] = checks
    #expect(throws: ContractError.self) { try diagnosticReport(body) }
    #expect(throws: ContractError.self) { try diagnosticReport(diagnosticBody(Array(repeating: "pass", count: 2049))) }
}

@Test func diagnosticDurationDoesNotRoundThroughFloatingPoint() throws {
    var body = diagnosticBody(["pass"]), checks = try #require(body["checks"] as? [[String: Any]])
    checks[0]["duration_ms"] = UInt64.max; body["checks"] = checks
    let report = try diagnosticReport(body)
    #expect(report.checks[0].durationMs == UInt64.max)
    #expect(report.text().contains("18446744073709551615 ms"))
}

@Test func diagnosticFiltersKeepExecutionOrderAndSearchRemediesLocally() throws {
    let report = try diagnosticReport()
    #expect(report.filtered(.attention).map(\.status) == [.warn, .fail])
    #expect(report.filtered(.skipped).map(\.status) == [.skip])
    #expect(report.filtered(.all, query: " HOST SETTINGS ").count == 4)
    #expect(report.filtered(.all, query: "check2").map(\.status) == [.fail])
    #expect(report.filtered(.all, query: "private-evidence-only").isEmpty)
}

@Test func diagnosticExportIncludesRemediesButEvidenceRequiresExplicitChoice() throws {
    let report = try diagnosticReport()
    let text = report.text()
    #expect(text.contains("Skipped") && text.contains("No repairs were performed"))
    #expect(text.contains("Suggested remedy: Review host settings"))
    #expect(!text.contains("private-evidence-only"))
    #expect(report.text(includeEvidence: true).contains("private-evidence-only"))
}

private actor DiagnosticTransport: DaemonTransport {
    var reply: HTTPReply
    var requests: [URLRequest] = []
    var held = false
    var waiting: [CheckedContinuation<Void, Never>] = []
    init(_ body: [String: Any] = diagnosticBody(), status: Int = 200) throws {
        reply = HTTPReply(data: try diagnosticData(body), status: status)
    }
    func respond(_ data: Data, status: Int = 200) { reply = HTTPReply(data: data, status: status) }
    func hold() { held = true }
    func release() { held = false; let work = waiting; waiting = []; for waiter in work { waiter.resume() } }
    func send(_ request: URLRequest) async throws -> HTTPReply {
        requests.append(request)
        let reply = reply
        // Deliberately ignore cancellation to exercise the production guards.
        if held { await withCheckedContinuation { waiting.append($0) } }
        return reply
    }
    nonisolated func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> {
        AsyncThrowingStream { $0.finish() }
    }
}
private func diagnosticClient(_ transport: DiagnosticTransport) throws -> DaemonClient {
    DaemonClient(endpoint: try DaemonEndpoint("https://example.invalid"), transport: transport)
}

@Test func diagnosticClientReadsOneFixedRouteWithoutAnyRoomOrPlaybackRequest() async throws {
    let transport = try DiagnosticTransport(), client = try diagnosticClient(transport)
    let report = try await client.diagnostics()
    let requests = await transport.requests
    #expect(requests.count == 1)
    let request = try #require(requests.first)
    #expect(request.httpMethod == "GET" && request.url?.absoluteString == "https://example.invalid/doctor")
    #expect(request.httpBody == nil && request.value(forHTTPHeaderField: "Accept") == "application/json")
    #expect(request.timeoutInterval == 180 && request.cachePolicy == .reloadIgnoringLocalCacheData)
    #expect(report.exitCode == 7) // diagnostic failures are a valid report, not a transport failure
}

@Test func diagnosticClientPreservesAPIReadErrorsWithoutRetryOrUncertainWrite() async throws {
    let transport = try DiagnosticTransport(["code": "POLICY_DENIED", "detail": "Read denied", "hint": "Ask the owner"], status: 403)
    do { _ = try await diagnosticClient(transport).diagnostics(); Issue.record("Expected policy error") }
    catch let error as APIProblem { #expect(error.code == "POLICY_DENIED" && error.hint == "Ask the owner") }
    #expect(await transport.requests.count == 1)
}

@Test func diagnosticClientRejectsMalformedSuccessAndPlainHTTPFailure() async throws {
    let transport = try DiagnosticTransport([:]), client = try diagnosticClient(transport)
    await #expect(throws: (any Error).self) { try await client.diagnostics() }
    await transport.respond(Data("unavailable".utf8), status: 503)
    do { _ = try await client.diagnostics(); Issue.record("Expected HTTP error") }
    catch let error as ClientError {
        guard case .http(503, _) = error else { Issue.record("A read was incorrectly reported as an uncertain write"); return }
    }
    #expect(await transport.requests.count == 2)
}

@Test @MainActor func diagnosticClientImmediateCancellationSendsNothing() async throws {
    let transport = try DiagnosticTransport(), client = try diagnosticClient(transport)
    let task = Task { try await client.diagnostics() }
    task.cancel()
    await #expect(throws: CancellationError.self) { try await task.value }
    #expect(await transport.requests.isEmpty)
}

@Test func diagnosticClientRejectsReplyAfterCancellationEvenIfTransportIgnoresIt() async throws {
    let transport = try DiagnosticTransport(), client = try diagnosticClient(transport)
    await transport.hold()
    let task = Task { try await client.diagnostics() }
    try await diagnosticEventually { await transport.waiting.count == 1 }
    task.cancel(); await transport.release()
    await #expect(throws: CancellationError.self) { try await task.value }
    #expect(await transport.requests.count == 1)
}

private func diagnosticEventually(_ condition: @Sendable () async -> Bool) async throws {
    let clock = ContinuousClock(), deadline = clock.now + .seconds(3)
    while clock.now < deadline {
        if await condition() { return }
        try await Task.sleep(for: .milliseconds(2))
    }
    throw ContractError.invalid("Diagnostic test observation timed out")
}

@Test @MainActor func diagnosticModelVisibilityNeverRunsOrRetriesChecks() async throws {
    let transport = try DiagnosticTransport(), model = DiagnosticsModel(client: try diagnosticClient(transport))
    #expect(!model.canRun)
    model.run(); model.resume(); model.resume(); model.suspend(); model.resume()
    await Task.yield()
    #expect(model.canRun && model.observation == nil)
    #expect(await transport.requests.isEmpty)
    model.stop()
}

@Test @MainActor func diagnosticModelExplicitRunPublishesValidatedReportAndReceiveTime() async throws {
    let report = try diagnosticReport(), time = Date(timeIntervalSince1970: 123)
    let model = DiagnosticsModel(read: { report }, now: { time })
    model.resume(); model.run()
    try await diagnosticEventually { await MainActor.run { !model.loading } }
    #expect(model.isCurrent && model.observation?.report == report)
    #expect(model.observation?.receivedAt == time)
    #expect(model.problem == nil)
    model.stop()
}

@Test @MainActor func diagnosticModelRepeatedRunDoesNotOverlapAnActiveRead() async throws {
    let transport = try DiagnosticTransport(), model = DiagnosticsModel(client: try diagnosticClient(transport))
    await transport.hold()
    model.resume(); model.run(); model.run(); model.run()
    try await diagnosticEventually { await transport.waiting.count == 1 }
    #expect(!model.canRun && model.loading)
    await transport.release()
    try await diagnosticEventually { await MainActor.run { !model.loading } }
    #expect(await transport.requests.count == 1)
    model.stop()
}

@Test @MainActor func diagnosticModelCancelBeforeTaskStartsDoesNotInvokeReader() async throws {
    let transport = try DiagnosticTransport(), model = DiagnosticsModel(client: try diagnosticClient(transport))
    model.resume(); model.run(); model.cancel()
    try await diagnosticEventually { await MainActor.run { !model.loading } }
    #expect(await transport.requests.isEmpty)
    #expect(!model.isCurrent && model.observation == nil)
    #expect(model.problem?.contains("daemon may finish") == true)
    model.stop()
}

@Test @MainActor func diagnosticModelCancellationDrainsBeforeAllowingAnotherRun() async throws {
    let transport = try DiagnosticTransport(), model = DiagnosticsModel(client: try diagnosticClient(transport))
    await transport.hold(); model.resume(); model.run()
    try await diagnosticEventually { await transport.waiting.count == 1 }
    model.cancel(); model.run(); model.resume(); model.run()
    #expect(!model.canRun)
    #expect(await transport.requests.count == 1)
    await transport.release()
    try await diagnosticEventually { await MainActor.run { !model.loading } }
    #expect(model.observation == nil && !model.isCurrent && model.canRun)
    model.run()
    try await diagnosticEventually { await MainActor.run { model.isCurrent } }
    #expect(await transport.requests.count == 2)
    model.stop()
}

@Test @MainActor func diagnosticModelFailedRefreshKeepsPreviousReportExplicitlyStale() async throws {
    let transport = try DiagnosticTransport(), model = DiagnosticsModel(client: try diagnosticClient(transport))
    model.resume(); model.run()
    try await diagnosticEventually { await MainActor.run { model.isCurrent } }
    let first = try #require(model.observation)
    await transport.respond(Data("invalid".utf8))
    model.run()
    #expect(!model.isCurrent)
    try await diagnosticEventually { await MainActor.run { !model.loading } }
    #expect(model.observation?.id == first.id && model.problem != nil && !model.isCurrent)
    model.suspend(); model.resume()
    #expect(await transport.requests.count == 2)
    model.stop()
}

@Test @MainActor func diagnosticModelSuspendRejectsLateResultsAndResumeDoesNotRerun() async throws {
    let transport = try DiagnosticTransport(), model = DiagnosticsModel(client: try diagnosticClient(transport))
    await transport.hold(); model.resume(); model.run()
    try await diagnosticEventually { await transport.waiting.count == 1 }
    model.suspend(); model.resume()
    #expect(model.loading && !model.canRun)
    await transport.release()
    try await diagnosticEventually { await MainActor.run { !model.loading } }
    #expect(model.observation == nil && model.problem == nil && !model.isCurrent)
    #expect(await transport.requests.count == 1)
    model.stop()
}

@Test @MainActor func diagnosticModelStopIsTerminalAndClearsPrivateReport() async throws {
    let transport = try DiagnosticTransport(), model = DiagnosticsModel(client: try diagnosticClient(transport))
    model.resume(); model.run()
    try await diagnosticEventually { await MainActor.run { model.isCurrent } }
    await transport.hold(); model.run()
    try await diagnosticEventually { await transport.waiting.count == 1 }
    model.stop(); model.resume(); model.run(); await transport.release()
    try await diagnosticEventually { await MainActor.run { !model.loading } }
    #expect(!model.active && !model.canRun && model.observation == nil && !model.isCurrent)
    #expect(await transport.requests.count == 2)
}
