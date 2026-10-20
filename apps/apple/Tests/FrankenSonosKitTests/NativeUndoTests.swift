import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

private let undoLargeID: Int64 = 9_007_199_254_740_993
private let undoIntent = "set volume in Studio@TEST-A to 24"
private func undoAction(_ patch: [String: JSONValue] = [:]) -> [String: JSONValue] {
    [
        "id": .integer(undoLargeID), "at": .integer(1_800_000_000),
        "client": .string("test-native-owner"), "surface": .string("http"),
        "intent": .string(undoIntent), "decision": .string("allow"),
        "result": .string("Volume changed"), "undoable": .bool(true)
    ].merging(patch) { _, new in new }
}
private func undoZone(_ household: String = "TEST-A") -> Zone {
    Zone(household: household, coordinatorRoom: "Studio", members: ["Studio"], transportState: "playing")
}
private func undoJSON<T: Encodable>(_ value: T, status: Int = 200, snake: Bool = false) throws -> HTTPReply {
    let encoder = JSONEncoder()
    if snake { encoder.keyEncodingStrategy = .convertToSnakeCase }
    return HTTPReply(data: try encoder.encode(value), status: status)
}
private func undoReceipt(_ patch: [String: JSONValue] = [:], removing: [String] = []) -> [String: JSONValue] {
    var row: [String: JSONValue] = [
        "summary": .string("Restored the saved room state"),
        "undone": .integer(undoLargeID), "intent": .string(undoIntent)
    ]
    row.merge(patch) { _, new in new }
    removing.forEach { row.removeValue(forKey: $0) }
    return row
}

/// Executes the production client and shared session against a controllable
/// daemon. A held request deliberately ignores cancellation until released.
private actor NativeUndoTransport: DaemonTransport {
    var requests: [URLRequest] = []
    var candidate: [String: JSONValue]? = undoAction()
    var topology = [undoZone(), undoZone("TEST-B")]
    var previewOverride: HTTPReply?
    var writeOverride: HTTPReply?
    var failedRead: String?
    var loseReply = false
    var cancelReply = false
    var failures: [String] = []
    var restores = 0
    var blocked = false
    private var blockKey: String?
    private var gate: CheckedContinuation<Void, Never>?

    func setCandidate(_ value: [String: JSONValue]?) { candidate = value }
    func setTopology(_ value: [Zone]) { topology = value }
    func overridePreview(_ value: HTTPReply?) { previewOverride = value }
    func overrideWrite(_ value: HTTPReply?) { writeOverride = value }
    func failRead(_ value: String?) { failedRead = value }
    func loseNextReply() { loseReply = true }
    func cancelNextReply() { cancelReply = true }
    func setFailures(_ value: [String]) { failures = value }
    func hold(_ method: String, _ path: String) { blockKey = method + " " + path }
    func release() { let saved = gate; gate = nil; blocked = false; saved?.resume() }
    func writes() -> [URLRequest] { requests.filter { $0.httpMethod == "POST" } }
    func send(_ request: URLRequest) async throws -> HTTPReply {
        requests.append(request)
        let method = request.httpMethod ?? "GET"
        let path = request.url!.path.removingPercentEncoding ?? request.url!.path
        let response: HTTPReply
        if method == "GET" {
            if failedRead == path { throw URLError(.cannotConnectToHost) }
            switch path {
            case "/undo/preview": response = try previewOverride ?? undoJSON(candidate)
            case "/zones": response = try undoJSON(topology, snake: true)
            case "/scenes": response = try undoJSON([Scene](), snake: true)
            case "/schedules": response = try undoJSON([scheduleFixture()])
            default:
                guard path.hasSuffix("/state"),
                      let zone = topology.first(where: { path == "/zones/" + $0.id.address + "/state" }) else {
                    throw ContractError.invalid("Unexpected undo test read: " + path)
                }
                response = try undoJSON(ZoneState(zone: zone, transportState: "playing", volume: 24,
                    mute: false, groupVolume: 24, track: nil, shuffle: false, repeat: "off"), snake: true)
            }
        } else if path == "/undo/checked" {
            let body = try JSONDecoder().decode([String: JSONValue].self, from: request.httpBody!)
            guard let candidate, body == ["expected_id": candidate["id"]!] else {
                throw ContractError.invalid("The checked undo did not name the current action exactly.")
            }
            restores += 1
            self.candidate = nil
            var receipt = undoReceipt(["undone": candidate["id"]!, "intent": candidate["intent"]!])
            if !failures.isEmpty { receipt["failures"] = .array(failures.map(JSONValue.string)) }
            response = try writeOverride ?? undoJSON(receipt)
        } else if path == "/pause" {
            response = try undoJSON(["done": JSONValue.string("Paused"), "changed": .bool(true)])
        } else {
            throw ContractError.invalid("Unexpected undo test write: " + path)
        }
        if blockKey == method + " " + path {
            blockKey = nil; blocked = true
            await withCheckedContinuation { gate = $0 }
        }
        if method == "POST" && loseReply { loseReply = false; throw URLError(.networkConnectionLost) }
        if method == "POST" && cancelReply { cancelReply = false; throw CancellationError() }
        return response
    }
    nonisolated func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> {
        AsyncThrowingStream { $0.finish() }
    }
}

private func undoClient(_ transport: any DaemonTransport) throws -> DaemonClient {
    DaemonClient(endpoint: try DaemonEndpoint("https://music.example.test"), transport: transport)
}
private func waitForUndoGate(_ transport: NativeUndoTransport) async throws {
    let deadline = ContinuousClock.now.advanced(by: .seconds(3))
    while !(await transport.blocked) {
        guard ContinuousClock.now < deadline else { throw ContractError.invalid("Undo test gate was not reached") }
        try await Task.sleep(for: .milliseconds(3))
    }
}
private func expectUndoUncertain(_ session: CommandReviewSession, _ review: UndoReview) async {
    do {
        _ = try await session.execute(review)
        Issue.record("A missing or contradictory undo acknowledgement was reported as success")
    } catch {
        if case ClientError.uncertain = error { }
        else { Issue.record("A dispatched undo must retain uncertainty: \(error)") }
    }
}

@Test func nativeUndoPreviewReadsOnlyTheAuthenticatedCallersCandidate() async throws {
    let transport = NativeUndoTransport(), client = try undoClient(transport)
    let candidate = try #require(try await client.undoPreview())
    #expect(candidate.id == undoLargeID && candidate.intent == undoIntent)
    #expect(candidate.client == "test-native-owner" && candidate.undoable && candidate.undoOf == nil)
    let requests = await transport.requests
    #expect(requests.count == 1)
    #expect(requests[0].httpMethod == "GET" && requests[0].url?.path == "/undo/preview")
    #expect(requests[0].url?.query == nil && requests[0].httpBody == nil)
    #expect(await transport.writes().isEmpty)
}

@Test func nativeUndoNoCandidateAndReadFailureAreDifferentFromPermissionToRestore() async throws {
    let transport = NativeUndoTransport(), client = try undoClient(transport)
    let session = CommandReviewSession(client: client)
    await transport.setCandidate(nil)
    #expect(try await client.undoPreview() == nil)
    await #expect(throws: ContractError.self) { try await session.prepareUndo() }
    await transport.failRead("/undo/preview")
    await #expect(throws: URLError.self) { try await client.undoPreview() }
    await #expect(throws: URLError.self) { try await session.prepareUndo() }
    #expect(await transport.writes().isEmpty)
}

@Test func nativeUndoMalformedOrIneligiblePreviewCannotBecomeAReview() async throws {
    for row in [
        undoAction(["id": .integer(0)]),
        undoAction(["at": .integer(Int64.max)]),
        undoAction(["undoable": .bool(false)]),
        undoAction(["undo_of": .integer(4)]),
        undoAction(["intent": .string(String(repeating: "x", count: 65_537))])
    ] {
        let transport = NativeUndoTransport(), session = try CommandReviewSession(client: undoClient(transport))
        await transport.setCandidate(row)
        await #expect(throws: (any Error).self) { try await session.prepareUndo() }
        #expect(await transport.writes().isEmpty)
    }
}

@Test func nativeUndoUnsupportedDaemonAndPolicyErrorsNeverFallBackToUncheckedUndo() async throws {
    for status in [403, 404, 501] {
        let transport = NativeUndoTransport(), session = try CommandReviewSession(client: undoClient(transport))
        await transport.overridePreview(HTTPReply(data: Data(#"{"code":"NOT_IMPLEMENTED","detail":"Undo unavailable","hint":"Use a daemon with checked undo"}"#.utf8), status: status))
        await #expect(throws: (any Error).self) { try await session.prepareUndo() }
        #expect(await transport.requests.map { $0.url!.path } == ["/undo/preview"])
        #expect(await transport.writes().isEmpty)
    }
}

@Test func nativeUndoChecksAgainThenRestoresExactFullWidthIDOnce() async throws {
    let transport = NativeUndoTransport(), session = try CommandReviewSession(client: undoClient(transport))
    await transport.failRead("/zones")
    let reviewed = try await session.prepareUndo()
    #expect(reviewed.action.id == undoLargeID)
    #expect(await transport.writes().isEmpty)
    let receipt = try await session.execute(reviewed)
    #expect(receipt.undone == undoLargeID && receipt.intent == undoIntent)
    #expect(!receipt.summary.isEmpty && receipt.failures.isEmpty)
    await #expect(throws: ContractError.self) { try await session.execute(reviewed) }
    #expect(await transport.restores == 1)
    #expect(await transport.requests.map { $0.url!.path } == ["/undo/preview", "/undo/preview", "/undo/checked"])
    let request = try #require(await transport.writes().first)
    let body = try JSONDecoder().decode([String: JSONValue].self, from: #require(request.httpBody))
    #expect(body == ["expected_id": .integer(undoLargeID)])
    #expect(request.url?.query == nil)
    #expect(request.value(forHTTPHeaderField: "Content-Type") == "application/json")
}

@Test func nativeUndoChangedCandidateOrCallerCannotBeSilentlyRetargeted() async throws {
    for replacement: [String: JSONValue]? in [
        nil,
        undoAction(["id": .integer(undoLargeID + 1)]),
        undoAction(["client": .string("another-client")]),
        undoAction(["surface": .string("cli")]),
        undoAction(["intent": .string("Change a different group")]),
        undoAction(["result": .string("A replacement record")]),
        undoAction(["decision": .string("clamp: changed policy")]),
        undoAction(["at": .integer(1_800_000_001)])
    ] {
        let transport = NativeUndoTransport(), session = try CommandReviewSession(client: undoClient(transport))
        let review = try await session.prepareUndo()
        await transport.setCandidate(replacement)
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes().isEmpty)
    }
}

@Test func nativeUndoFailedFinalObservationConsumesReviewWithoutDispatch() async throws {
    let transport = NativeUndoTransport(), session = try CommandReviewSession(client: undoClient(transport))
    let review = try await session.prepareUndo()
    await transport.failRead("/undo/preview")
    await #expect(throws: URLError.self) { try await session.execute(review) }
    await transport.failRead(nil)
    await #expect(throws: ContractError.self) { try await session.execute(review) }
    #expect(await transport.writes().isEmpty)
}

@Test func nativeUndoPlaybackAndSchedulesShareOnePendingReview() async throws {
    let transport = NativeUndoTransport(), client = try undoClient(transport), session = CommandReviewSession(client: client)
    let playback = try await session.prepare(.pause, in: undoZone())
    let undo = try await session.prepareUndo()
    await #expect(throws: ContractError.self) { try await session.execute(playback) }
    let scheduled = try await session.prepare(ScheduleCommand.change(scheduleDecode(scheduleFixture()), .pause))
    await #expect(throws: ContractError.self) { try await session.execute(undo) }
    let replacement = try await session.prepareUndo()
    await #expect(throws: ContractError.self) { try await session.execute(scheduled) }
    _ = try await session.execute(replacement)
    #expect(await transport.writes().count == 1)
}

@Test func nativeUndoDoubleConfirmationAndOtherCommandTypesCannotOverlapDispatch() async throws {
    let transport = NativeUndoTransport(), session = try CommandReviewSession(client: undoClient(transport))
    let review = try await session.prepareUndo()
    await transport.hold("POST", "/undo/checked")
    let sent = Task { try await session.execute(review) }
    try await waitForUndoGate(transport)
    await #expect(throws: ContractError.self) { try await session.execute(review) }
    await #expect(throws: ContractError.self) { try await session.prepareUndo() }
    await #expect(throws: ContractError.self) { try await session.prepare(.pause, in: undoZone()) }
    await #expect(throws: ContractError.self) { try await session.prepare(ScheduleCommand.change(scheduleDecode(scheduleFixture()), .pause)) }
    await transport.release()
    #expect(try await sent.value.undone == undoLargeID)
    #expect(await transport.writes().count == 1)
    #expect(await transport.restores == 1)
}

@Test func nativeUndoCancelledPreparationCannotCreateALateReviewOrFreeABusySlot() async throws {
    let transport = NativeUndoTransport(), session = try CommandReviewSession(client: undoClient(transport))
    await transport.hold("GET", "/undo/preview")
    let preparing = Task { try await session.prepareUndo() }
    try await waitForUndoGate(transport)
    await session.cancel()
    await #expect(throws: ContractError.self) { try await session.prepareUndo() }
    await #expect(throws: ContractError.self) { try await session.prepare(.pause, in: undoZone()) }
    await transport.release()
    await #expect(throws: CancellationError.self) { try await preparing.value }
    #expect(await transport.writes().isEmpty)
    _ = try await session.prepareUndo()
}

@Test func nativeUndoCancellationAfterDelayedFinalReadPreventsDispatch() async throws {
    for cancelTask in [true, false] {
        let transport = NativeUndoTransport(), session = try CommandReviewSession(client: undoClient(transport))
        let review = try await session.prepareUndo()
        await transport.hold("GET", "/undo/preview")
        let confirming = Task { try await session.execute(review) }
        try await waitForUndoGate(transport)
        if cancelTask { confirming.cancel() } else { await session.cancel() }
        await #expect(throws: ContractError.self) { try await session.prepareUndo() }
        await transport.release()
        await #expect(throws: CancellationError.self) { try await confirming.value }
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes().isEmpty)
    }
}

@Test func nativeUndoForeignAndCancelledReviewsCannotExecute() async throws {
    let transport = NativeUndoTransport(), client = try undoClient(transport)
    let session = CommandReviewSession(client: client), other = CommandReviewSession(client: client)
    let review = try await session.prepareUndo()
    await #expect(throws: ContractError.self) { try await other.execute(review) }
    await session.cancel()
    await #expect(throws: ContractError.self) { try await session.execute(review) }
    #expect(await transport.writes().isEmpty)
}

@Test func nativeUndoAllPartialRestorationFailuresSurviveTheReceipt() async throws {
    let transport = NativeUndoTransport(), session = try CommandReviewSession(client: undoClient(transport))
    let failures = ["Hall@TEST-A: speaker unavailable", "Studio@TEST-A Position: queue changed", "DJ steering was not restored"]
    await transport.setFailures(failures)
    let receipt = try await session.execute(session.prepareUndo())
    #expect(receipt.failures == failures && receipt.undone == undoLargeID)
    #expect(await transport.writes().count == 1)
}

@Test func nativeUndoMalformedOrContradictoryReceiptsRemainUncertainWithoutRetry() async throws {
    let corrupt = [
        undoReceipt(removing: ["undone"]),
        undoReceipt(removing: ["intent"]),
        undoReceipt(["undone": .integer(undoLargeID + 1)]),
        undoReceipt(["undone": .string(String(undoLargeID))]),
        undoReceipt(["intent": .string("Another action")]),
        undoReceipt(["summary": .string("")]),
        undoReceipt(["summary": .string(String(repeating: "x", count: 65_537))]),
        undoReceipt(["failures": .array([.integer(1)])])
    ]
    for row in corrupt {
        let transport = NativeUndoTransport(), session = try CommandReviewSession(client: undoClient(transport))
        let review = try await session.prepareUndo()
        await transport.overrideWrite(try undoJSON(row))
        await expectUndoUncertain(session, review)
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes().count == 1)
    }
}

@Test func nativeUndoLostAcknowledgementAndTransportCancellationNeverRepeatTheRestore() async throws {
    for cancellation in [true, false] {
        let transport = NativeUndoTransport(), session = try CommandReviewSession(client: undoClient(transport))
        let review = try await session.prepareUndo()
        if cancellation { await transport.cancelNextReply() } else { await transport.loseNextReply() }
        await expectUndoUncertain(session, review)
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        await session.cancel()
        #expect(await transport.writes().count == 1)
        #expect(await transport.restores == 1)
    }
}

@Test func nativeUndoPostSendCancellationRetainsAnAuthoritativeLateReceipt() async throws {
    let transport = NativeUndoTransport(), session = try CommandReviewSession(client: undoClient(transport))
    let review = try await session.prepareUndo()
    await transport.hold("POST", "/undo/checked")
    let sent = Task { try await session.execute(review) }
    try await waitForUndoGate(transport)
    sent.cancel(); await session.cancel()
    await #expect(throws: ContractError.self) { try await session.prepareUndo() }
    await transport.release()
    #expect(try await sent.value.undone == undoLargeID)
    await #expect(throws: ContractError.self) { try await session.execute(review) }
    #expect(await transport.writes().count == 1)
}

private final class UndoScreenTransport: DaemonTransport, Sendable {
    let base = NativeUndoTransport()
    let stream: AsyncThrowingStream<EventSignal, Error>
    let output: AsyncThrowingStream<EventSignal, Error>.Continuation
    init() {
        let pair = AsyncThrowingStream<EventSignal, Error>.makeStream()
        stream = pair.stream; output = pair.continuation
    }
    func send(_ request: URLRequest) async throws -> HTTPReply { try await base.send(request) }
    func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> {
        output.yield(.connected); return stream
    }
}
@MainActor private func undoScreenWait(_ condition: () -> Bool) async throws {
    let deadline = ContinuousClock.now.advanced(by: .seconds(3))
    while !condition() {
        guard ContinuousClock.now < deadline else { throw ContractError.invalid("Undo screen did not settle") }
        try await Task.sleep(for: .milliseconds(5))
    }
}
@MainActor private func undoScreen(_ transport: UndoScreenTransport, waitForRooms: Bool = true) async throws -> RemoteModel {
    let model = RemoteModel(client: try undoClient(transport))
    model.show(.scenes); model.resume()
    try await undoScreenWait { model.canSchedule && (!waitForRooms || model.selectedZone != nil) }
    return model
}

@Test @MainActor func nativeUndoScreenCanRecoverWithoutASelectedOrReachableRoom() async throws {
    let transport = UndoScreenTransport()
    await transport.base.failRead("/zones")
    let model = try await undoScreen(transport, waitForRooms: false)
    defer { model.stop() }
    #expect(model.selectedRoom == nil && !model.canPrepare)
    model.prepareUndo()
    try await undoScreenWait { model.undoReview != nil }
    #expect(model.undoReview?.action.id == undoLargeID && model.review == nil && model.scheduleReview == nil)
    let id = try #require(model.undoReview?.id)
    model.confirmUndo(id); model.confirmUndo(id)
    try await undoScreenWait { model.undoResult != nil && model.actionPhase == .idle }
    #expect(model.undoResult?.kind == .confirmed)
    #expect(model.undoResult?.action?.id == undoLargeID && model.undoResult?.receipt?.undone == undoLargeID)
    #expect(await transport.base.writes().count == 1)
}

@Test @MainActor func nativeUndoScreenKeepsEveryPartialFailureAndItsOriginalAction() async throws {
    let transport = UndoScreenTransport(), model = try await undoScreen(transport)
    defer { model.stop() }
    let failures = ["Hall@TEST-A: unavailable", "Studio@TEST-A Position: queue changed"]
    await transport.base.setFailures(failures)
    model.prepareUndo()
    try await undoScreenWait { model.undoReview != nil }
    model.confirmUndo(try #require(model.undoReview?.id))
    try await undoScreenWait { model.undoResult != nil && model.actionPhase == .idle }
    #expect(model.undoResult?.kind == .partial && model.undoResult?.receipt?.failures == failures)
    #expect(model.undoResult?.action?.intent == undoIntent)
}

@Test @MainActor func nativeUndoScreenMissingCandidateProducesANotSentResultWithoutATarget() async throws {
    let transport = UndoScreenTransport(), model = try await undoScreen(transport)
    defer { model.stop() }
    await transport.base.setCandidate(nil)
    model.prepareUndo()
    try await undoScreenWait { model.undoResult != nil && model.actionPhase == .idle }
    #expect(model.undoResult?.kind == .notSent && model.undoResult?.action == nil)
    #expect(model.undoReview == nil && model.result == nil && model.scheduleResult == nil)
    #expect(await transport.base.writes().isEmpty)
}

@Test @MainActor func nativeUndoScreenAChangedActionRequiresANewExplicitReview() async throws {
    let transport = UndoScreenTransport(), model = try await undoScreen(transport)
    defer { model.stop() }
    model.prepareUndo()
    try await undoScreenWait { model.undoReview != nil }
    let old = try #require(model.undoReview?.id)
    await transport.base.setCandidate(undoAction(["id": .integer(undoLargeID + 1)]))
    model.confirmUndo(old)
    try await undoScreenWait { model.undoResult != nil && model.actionPhase == .idle }
    #expect(model.undoResult?.kind == .notSent && model.undoResult?.action?.id == undoLargeID)
    model.confirmUndo(old); model.refresh()
    #expect(model.undoReview == nil)
    #expect(await transport.base.writes().isEmpty)
    model.prepareUndo()
    try await undoScreenWait { model.undoReview != nil }
    #expect(model.undoReview?.action.id == undoLargeID + 1)
}

@Test @MainActor func nativeUndoScreenCancellationDuringPreparationCannotReopenAReview() async throws {
    let transport = UndoScreenTransport(), model = try await undoScreen(transport)
    defer { model.stop() }
    await transport.base.hold("GET", "/undo/preview")
    model.prepareUndo()
    try await waitForUndoGate(transport.base)
    model.cancelUndo(); model.prepare(.pause); model.prepareUndo()
    #expect(model.actionPhase == .preparing && model.review == nil && model.undoReview == nil)
    await transport.base.release()
    try await undoScreenWait { model.actionPhase == .idle }
    #expect(model.undoReview == nil && model.undoResult == nil)
    #expect(await transport.base.writes().isEmpty)
}

@Test @MainActor func nativeUndoScreenDismissalAndSuspensionFenceTheFinalRead() async throws {
    for suspend in [true, false] {
        let transport = UndoScreenTransport(), model = try await undoScreen(transport)
        defer { model.stop() }
        model.prepareUndo()
        try await undoScreenWait { model.undoReview != nil }
        let id = try #require(model.undoReview?.id)
        await transport.base.hold("GET", "/undo/preview")
        model.confirmUndo(id)
        try await waitForUndoGate(transport.base)
        if suspend { model.suspend() } else { model.cancelUndo() }
        await transport.base.release()
        try await undoScreenWait { model.actionPhase == .idle }
        #expect(model.undoResult?.kind == .notSent && model.undoResult?.action?.id == undoLargeID)
        #expect(model.undoReview == nil)
        model.confirmUndo(id)
        #expect(await transport.base.writes().isEmpty)
    }
}

@Test @MainActor func nativeUndoScreenLateReceiptStaysWithItsReviewedActionAcrossSelectionOrSuspension() async throws {
    for suspend in [true, false] {
        let transport = UndoScreenTransport(), model = try await undoScreen(transport)
        defer { model.stop() }
        model.prepareUndo()
        try await undoScreenWait { model.undoReview != nil }
        await transport.base.hold("POST", "/undo/checked")
        model.confirmUndo(try #require(model.undoReview?.id))
        try await waitForUndoGate(transport.base)
        if suspend { model.suspend() } else { model.select(undoZone("TEST-B").id) }
        await transport.base.release()
        try await undoScreenWait { model.undoResult != nil && model.actionPhase == .idle }
        #expect(model.undoResult?.kind == .confirmed && model.undoResult?.action?.id == undoLargeID)
        #expect(model.undoResult?.action?.intent == undoIntent && model.undoResult?.receipt?.undone == undoLargeID)
        if !suspend { #expect(model.selectedRoom == undoZone("TEST-B").id) }
        #expect(model.result == nil && model.scheduleResult == nil)
        #expect(await transport.base.writes().count == 1)
    }
}

@Test @MainActor func nativeUndoScreenUnrelatedTopologyEventsDoNotCancelARecoveryReview() async throws {
    let transport = UndoScreenTransport(), model = try await undoScreen(transport)
    defer { model.stop() }
    model.prepareUndo()
    try await undoScreenWait { model.undoReview != nil }
    let id = try #require(model.undoReview?.id)
    await transport.base.setTopology([undoZone("TEST-B")])
    transport.output.yield(.event(ServerEvent(id: "undo-topology", kind: "topology.changed", data: "{}")))
    try await undoScreenWait { model.selectedZone == nil }
    #expect(model.undoReview?.id == id)
    model.confirmUndo(id)
    try await undoScreenWait { model.undoResult != nil && model.actionPhase == .idle }
    #expect(model.undoResult?.kind == .confirmed)
    #expect(await transport.base.writes().count == 1)
}

@Test @MainActor func nativeUndoScreenPlaybackScheduleAndUndoControlsShareOneGate() async throws {
    let transport = UndoScreenTransport(), model = try await undoScreen(transport)
    defer { model.stop() }
    model.prepareUndo()
    try await undoScreenWait { model.undoReview != nil }
    model.prepare(.pause)
    model.prepareSchedule(.change(try scheduleDecode(scheduleFixture()), .pause))
    #expect(model.review == nil && model.scheduleReview == nil && !model.canPrepare && !model.canSchedule)
    await transport.base.hold("POST", "/undo/checked")
    model.confirmUndo(try #require(model.undoReview?.id))
    try await waitForUndoGate(transport.base)
    model.prepare(.pause); model.prepareUndo()
    #expect(model.actionPhase == .confirming && model.undoReview == nil && model.review == nil)
    await transport.base.release()
    try await undoScreenWait { model.actionPhase == .idle }
    model.prepare(.pause)
    try await undoScreenWait { model.review != nil }
    model.prepareUndo(); model.cancelUndo()
    #expect(model.review != nil && model.undoReview == nil)
    #expect(await transport.base.writes().count == 1)
}

@Test @MainActor func nativeUndoScreenLostAcknowledgementSurvivesRefreshAndNeverReplays() async throws {
    let transport = UndoScreenTransport(), model = try await undoScreen(transport)
    defer { model.stop() }
    model.prepareUndo()
    try await undoScreenWait { model.undoReview != nil }
    let id = try #require(model.undoReview?.id)
    await transport.base.loseNextReply()
    model.confirmUndo(id)
    try await undoScreenWait { model.undoResult?.kind == .uncertain && model.actionPhase == .idle }
    model.confirmUndo(id); model.refresh(); model.suspend(); model.resume()
    try await undoScreenWait { model.canSchedule }
    #expect(model.undoResult?.kind == .uncertain && model.undoResult?.action?.id == undoLargeID)
    #expect(model.undoReview == nil && model.undoResult?.receipt == nil)
    #expect(await transport.base.writes().count == 1)
    #expect(await transport.base.restores == 1)
    model.dismissUndoResult()
    #expect(model.undoResult == nil)
    #expect(await transport.base.writes().count == 1)
}
