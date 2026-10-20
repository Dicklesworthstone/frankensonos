import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

func diagnosticFixture() -> [String: JSONValue] {
    func check(_ id: String, _ status: String, _ summary: String) -> JSONValue {
        .object(["id": .string(id), "title": .string(id.capitalized), "status": .string(status),
                 "summary": .string(summary), "duration_ms": .integer(3)])
    }
    return ["schema": .integer(1), "exit_code": .integer(7),
            "counts": .object(["pass": .integer(1), "warn": .integer(1), "fail": .integer(1), "skip": .integer(1)]),
            "checks": .array([check("network", "pass", "Host answered"), check("spotify", "warn", "Library grant needs attention"),
                              check("callback", "fail", "Callback unreachable"), check("playback", "skip", "Prerequisite unavailable")])]
}
func diagnosticDecode(_ value: [String: JSONValue]) throws -> DiagnosticReport {
    try DaemonClient.decode(DiagnosticReport.self, from: JSONEncoder().encode(value)).validated()
}
actor DiagnosticFixtureTransport: DaemonTransport {
    private var reply = HTTPReply(data: Data(), status: 200)
    private(set) var requests: [URLRequest] = []
    private(set) var waiting = false
    private var blocked = false
    private var continuation: CheckedContinuation<Void, Never>?
    func set(_ value: [String: JSONValue], status: Int = 200) throws {
        reply = HTTPReply(data: try JSONEncoder().encode(value), status: status)
    }
    func setRaw(_ data: Data) { reply = HTTPReply(data: data, status: 200) }
    func hold() { blocked = true }
    func release() { let gate = continuation; continuation = nil; waiting = false; gate?.resume() }
    func send(_ request: URLRequest) async throws -> HTTPReply {
        requests.append(request)
        let captured = reply
        if blocked {
            blocked = false; waiting = true
            await withCheckedContinuation { continuation = $0 }
        }
        return captured // Deliberately ignore cancellation to test fencing.
    }
    nonisolated func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> {
        AsyncThrowingStream { $0.finish(throwing: ContractError.invalid("Unexpected diagnostic event subscription")) }
    }
}
@MainActor func diagnosticModel(_ transport: DiagnosticFixtureTransport, deadline: Duration = .seconds(120)) throws -> DiagnosticsModel {
    DiagnosticsModel(endpoint: try DaemonEndpoint("https://daemon.example.test"), transport: transport, deadline: deadline)
}
@MainActor func diagnosticEventually(_ condition: () -> Bool) async throws {
    let limit = ContinuousClock.now.advanced(by: .seconds(2))
    while !condition() {
        guard ContinuousClock.now < limit else { throw ContractError.invalid("Diagnostic model did not settle") }
        try await Task.sleep(for: .milliseconds(2))
    }
}
func diagnosticGate(_ transport: DiagnosticFixtureTransport) async throws {
    let limit = ContinuousClock.now.advanced(by: .seconds(2))
    while !(await transport.waiting) {
        guard ContinuousClock.now < limit else { throw ContractError.invalid("Diagnostic request did not reach its gate") }
        try await Task.sleep(for: .milliseconds(2))
    }
}

@Test func diagnosticReportPreservesPrerequisiteOrderAndRealTotals() throws {
    let report = try diagnosticDecode(diagnosticFixture())
    #expect(report.checks.map(\.id) == ["network", "spotify", "callback", "playback"])
    #expect(report.counts == ["pass": 1, "warn": 1, "fail": 1, "skip": 1])
    #expect(report.exitCode == 7 && report.visible().map(\.status) == [.warn, .fail, .skip])
}

@Test func diagnosticReportRejectsWrongVersionCountsAndExitStatus() throws {
    for (field, value) in [("schema", JSONValue.integer(2)), ("exit_code", .integer(0)),
                           ("counts", .object(["pass": .integer(4), "warn": .integer(0), "fail": .integer(0), "skip": .integer(0)])),
                           ("counts", .object(["pass": .integer(1), "warn": .integer(1), "fail": .integer(1)]))] {
        var input = diagnosticFixture(); input[field] = value
        #expect(throws: (any Error).self) { try diagnosticDecode(input) }
    }
}

@Test func diagnosticReportRejectsRepeatedMissingAndUnknownCheckFields() throws {
    for field in ["id", "title", "status", "duration_ms"] {
        var input = diagnosticFixture()
        guard case .array(var rows) = input["checks"], case .object(var row) = rows[0] else { throw ClientError.unreadable }
        row.removeValue(forKey: field); rows[0] = .object(row); input["checks"] = .array(rows)
        #expect(throws: (any Error).self) { try diagnosticDecode(input) }
    }
    for (field, value) in [("id", JSONValue.string("spotify")), ("status", .string("unknown")), ("duration_ms", .integer(-1)), ("title", .string("  "))] {
        var input = diagnosticFixture()
        guard case .array(var rows) = input["checks"], case .object(var row) = rows[0] else { throw ClientError.unreadable }
        row[field] = value; rows[0] = .object(row); input["checks"] = .array(rows)
        #expect(throws: (any Error).self) { try diagnosticDecode(input) }
    }
}

@Test func diagnosticEmptyAndSkippedReportsAreNotInventedPasses() throws {
    var input = diagnosticFixture()
    input["checks"] = .array([])
    input["counts"] = .object(["pass": .integer(0), "warn": .integer(0), "fail": .integer(0), "skip": .integer(0)])
    input["exit_code"] = .integer(0)
    #expect(try diagnosticDecode(input).checks.isEmpty)
    input["checks"] = .array([.object(["id": .string("skip"), "title": .string("Skipped"), "status": .string("skip"), "summary": .string("Prerequisite"), "duration_ms": .integer(0)])])
    input["counts"] = .object(["pass": .integer(0), "warn": .integer(0), "fail": .integer(0), "skip": .integer(1)])
    #expect(try diagnosticDecode(input).counts["pass"] == 0)
}

@Test func diagnosticEvidenceAndRemediesRemainLiteralData() throws {
    var input = diagnosticFixture()
    guard case .array(var rows) = input["checks"], case .object(var row) = rows[2] else { throw ClientError.unreadable }
    row["remedy"] = .string("<script>not a command</script> https://example.test/fix")
    row["evidence"] = .object(["attempts": .integer(9_007_199_254_740_993), "detail": .array([.string("Test evidence"), .bool(false)])])
    rows[2] = .object(row); input["checks"] = .array(rows)
    let report = try diagnosticDecode(input)
    #expect(report.checks[2].remedy?.hasPrefix("<script>") == true)
    #expect(report.checks[2].evidenceText?.contains("9007199254740993") == true)
    #expect(report.checks[0].evidenceText == nil)
}

@Test func diagnosticFilteringIsLocalAndIncludesSkippedPrerequisites() throws {
    let report = try diagnosticDecode(diagnosticFixture())
    #expect(report.visible(filter: .all, query: "HOST").map(\.id) == ["network"])
    #expect(report.visible(filter: .attention, query: "PREREQUISITE").map(\.status) == [.skip])
    #expect(report.visible(filter: .fail).map(\.id) == ["callback"])
    #expect(report.visible(filter: .pass).map(\.id) == ["network"])
}

@Test func diagnosticReportLimitsChecksAndText() throws {
    var input = diagnosticFixture()
    guard case .array(var rows) = input["checks"], case .object(var row) = rows[0] else { throw ClientError.unreadable }
    row["detail"] = .string(String(repeating: "x", count: 65_537)); rows[0] = .object(row); input["checks"] = .array(rows)
    #expect(throws: ContractError.self) { try diagnosticDecode(input) }
    input = diagnosticFixture(); input["checks"] = .array(Array(repeating: rows[1], count: 2_049))
    #expect(throws: ContractError.self) { try diagnosticDecode(input) }
}

@Test @MainActor func diagnosticOpeningForegroundingAndClosingNeverRunChecks() async throws {
    let transport = DiagnosticFixtureTransport(), model = try diagnosticModel(transport)
    model.resume(); model.suspend(); model.resume(); model.stop(); model.run(); model.resume()
    try await Task.sleep(for: .milliseconds(20))
    #expect(await transport.requests.isEmpty)
    #expect(!model.active && !model.canRun)
}

@Test @MainActor func diagnosticExplicitRunReadsOnlyFixedGETAndPublishesReport() async throws {
    let transport = DiagnosticFixtureTransport(); try await transport.set(diagnosticFixture())
    let model = try diagnosticModel(transport); defer { model.stop() }
    model.resume(); model.run()
    try await diagnosticEventually { !model.running }
    #expect(model.report?.checks.count == 4 && model.receivedAt != nil && !model.stale)
    let requests = await transport.requests
    #expect(requests.count == 1)
    let request = try #require(requests.first)
    #expect(request.url?.absoluteString == "https://daemon.example.test/doctor")
    #expect((request.httpMethod ?? "GET") == "GET" && request.httpBody == nil)
    #expect(request.timeoutInterval == 120)
    #expect(request.value(forHTTPHeaderField: "Authorization") == nil)
}

@Test @MainActor func diagnosticReadFailureKeepsPriorReportExplicitlyStale() async throws {
    let transport = DiagnosticFixtureTransport(); try await transport.set(diagnosticFixture())
    let model = try diagnosticModel(transport); defer { model.stop() }
    model.resume(); model.run(); try await diagnosticEventually { !model.running }
    let firstDate = model.receivedAt
    try await transport.set(["code": .string("POLICY_DENIED"), "detail": .string("Not permitted"), "hint": .string("Review daemon policy")], status: 403)
    model.run(); try await diagnosticEventually { !model.running }
    #expect(model.report?.checks.count == 4 && model.stale && model.receivedAt == firstDate)
    #expect(model.problem?.contains("Review daemon policy") == true)
    try await Task.sleep(for: .milliseconds(25))
    #expect(await transport.requests.count == 2)
}

@Test @MainActor func diagnosticFirstReadFailureDoesNotBecomeAnEmptySuccess() async throws {
    let transport = DiagnosticFixtureTransport(); await transport.setRaw(Data("incomplete".utf8))
    let model = try diagnosticModel(transport); defer { model.stop() }
    model.resume(); model.run(); try await diagnosticEventually { !model.running }
    #expect(model.report == nil && model.receivedAt == nil && model.problem != nil)
}

@Test @MainActor func diagnosticOversizedResponseIsRejectedBeforeReportDecoding() async throws {
    let transport = DiagnosticFixtureTransport(); await transport.setRaw(Data(repeating: 32, count: DiagnosticReport.maximumBytes + 1))
    let model = try diagnosticModel(transport); defer { model.stop() }
    model.resume(); model.run(); try await diagnosticEventually { !model.running }
    #expect(model.problem?.contains("2 MiB") == true && model.report == nil)
}

@Test @MainActor func diagnosticDoubleClickDoesNotQueueAnotherExpensiveRead() async throws {
    let transport = DiagnosticFixtureTransport(); try await transport.set(diagnosticFixture()); await transport.hold()
    let model = try diagnosticModel(transport); defer { model.stop() }
    model.resume(); model.run(); try await diagnosticGate(transport)
    for _ in 0..<20 { model.run() }
    await transport.release(); try await diagnosticEventually { !model.running }
    #expect(await transport.requests.count == 1)
    #expect(model.report != nil)
}

@Test @MainActor func diagnosticRapidReopenWaitsForCancelledReadAndRejectsItsResult() async throws {
    let transport = DiagnosticFixtureTransport(); try await transport.set(diagnosticFixture()); await transport.hold()
    let model = try diagnosticModel(transport); defer { model.stop() }
    model.resume(); model.run(); try await diagnosticGate(transport)
    model.suspend(); model.resume(); model.run()
    #expect(model.running && !model.canRun)
    #expect(await transport.requests.count == 1)
    await transport.release(); try await diagnosticEventually { !model.running }
    #expect(model.report == nil && model.receivedAt == nil)
    model.run(); try await diagnosticEventually { !model.running }
    #expect(model.report != nil && !model.stale)
    #expect(await transport.requests.count == 2)
}

@Test @MainActor func diagnosticDeadlineReturnsFeedbackButRetainsTheBusySlot() async throws {
    let transport = DiagnosticFixtureTransport(); try await transport.set(diagnosticFixture()); await transport.hold()
    let model = try diagnosticModel(transport, deadline: .milliseconds(20)); defer { model.stop() }
    model.resume(); model.run(); try await diagnosticGate(transport)
    try await diagnosticEventually { model.problem?.contains("deadline") == true }
    #expect(model.running && !model.canRun)
    model.run(); await transport.release(); try await diagnosticEventually { !model.running }
    #expect(model.report == nil)
    #expect(await transport.requests.count == 1)
}

@Test @MainActor func diagnosticCancellationLeavesPreviousEvidenceButNotFreshness() async throws {
    let transport = DiagnosticFixtureTransport(); try await transport.set(diagnosticFixture())
    let model = try diagnosticModel(transport); defer { model.stop() }
    model.resume(); model.run(); try await diagnosticEventually { !model.running }
    let old = model.report
    await transport.hold(); model.run(); try await diagnosticGate(transport)
    model.cancel(); await transport.release(); try await diagnosticEventually { !model.running }
    #expect(model.report == old && model.stale)
    #expect(model.problem?.contains("cancelled") == true)
}

@Test @MainActor func diagnosticStoppedModelCannotPublishLateResultsOrRestart() async throws {
    let transport = DiagnosticFixtureTransport(); try await transport.set(diagnosticFixture()); await transport.hold()
    let model = try diagnosticModel(transport)
    model.resume(); model.run(); try await diagnosticGate(transport)
    model.stop(); model.resume(); model.run(); await transport.release()
    try await diagnosticEventually { !model.running }
    #expect(model.report == nil && !model.active && !model.canRun)
    #expect(await transport.requests.count == 1)
}

@Test @MainActor func diagnosticViewObservationDoesNotOwnOrTriggerTheRead() async throws {
    let transport = DiagnosticFixtureTransport(); try await transport.set(diagnosticFixture())
    let model = try diagnosticModel(transport); defer { model.stop() }
    var changes = 0; model.onChange = { changes += 1 }
    model.resume(); #expect(changes > 0)
    #expect(await transport.requests.isEmpty)
    model.run(); try await diagnosticEventually { !model.running }
    #expect(changes >= 6)
}
