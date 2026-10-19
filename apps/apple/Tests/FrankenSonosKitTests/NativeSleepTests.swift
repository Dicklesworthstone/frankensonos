import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

actor NativeSleepTransport: DaemonTransport {
    let house = NativeAutomationTransport()
    var requests: [URLRequest] = []
    var timers: [String: SleepTimer] = [:]
    var failReads = false, loseWrites = false, speakerOnly = false
    var readBody: String?, writeBody: String?
    private var holdMethod: String?
    private var continuation: CheckedContinuation<Void, Never>?
    var blocked = false

    func set(_ timer: SleepTimer?, at address: String = "Workshop@TEST-A") { timers[address] = timer }
    func failRead(_ flag: Bool = true) { failReads = flag }
    func loseWrite() { loseWrites = true }
    func speakerTimer() { speakerOnly = true }
    func overrideRead(_ value: String) { readBody = value }
    func overrideWrite(_ value: String) { writeBody = value }
    func hold(_ method: String) { holdMethod = method; blocked = false }
    func release() { let saved = continuation; continuation = nil; blocked = false; saved?.resume() }
    func writes() -> [URLRequest] { requests.filter { $0.httpMethod == "POST" } }
    func timerReads() -> Int { requests.filter { $0.url?.path == "/sleep" && $0.httpMethod != "POST" }.count }

    func send(_ request: URLRequest) async throws -> HTTPReply {
        requests.append(request)
        guard request.url?.path == "/sleep" else { return try await house.send(request) }
        let method = request.httpMethod ?? "GET"
        let reply: HTTPReply
        if method == "GET" {
            if failReads { throw URLError(.cannotConnectToHost) }
            let query = URLComponents(url: request.url!, resolvingAgainstBaseURL: false)!.queryItems!
            let address = try #require(query.first { $0.name == "zone" }?.value)
            if let readBody { reply = HTTPReply(data: Data(readBody.utf8), status: 200) }
            else { reply = try json(timers[address].map { [$0] } ?? []) }
        } else {
            if loseWrites { throw URLError(.networkConnectionLost) }
            if let writeBody { reply = HTTPReply(data: Data(writeBody.utf8), status: 200) }
            else {
                let body = try JSONDecoder().decode([String: JSONValue].self, from: request.httpBody!)
                guard case .string(let address) = body["zone"] else { throw ContractError.invalid("Missing test target") }
                let name = String(address.split(separator: "@")[0])
                if body["cancel"] == .bool(true) {
                    let had = timers.removeValue(forKey: address) != nil
                    reply = try json(sampleSleep(zone: name, ends: nil, remaining: nil, fades: false,
                                                 done: "Timer cancelled", changed: had))
                } else {
                    guard case .string(let raw) = body["duration"], raw.hasSuffix("s"),
                          let seconds = UInt64(raw.dropLast()) else { throw ContractError.invalid("Missing test duration") }
                    let previous = body["extend"] == .bool(true) ? timers[address] : nil
                    let remaining = (previous?.remainingSecs ?? 0) + seconds
                    let formatter = ISO8601DateFormatter()
                    let end = (previous?.deadline ?? Date()).addingTimeInterval(Double(seconds))
                    let timer = sampleSleep(zone: name, ends: formatter.string(from: end), remaining: remaining,
                                            fades: !speakerOnly, done: "Sleep timer set", changed: true)
                    timers[address] = timer
                    reply = try json(timer)
                }
            }
        }
        if holdMethod == method {
            holdMethod = nil; blocked = true
            await withCheckedContinuation { continuation = $0 }
        }
        return reply
    }
    nonisolated func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> {
        AsyncThrowingStream { $0.yield(.connected) }
    }
    private func json<T: Encodable>(_ value: T) throws -> HTTPReply {
        let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
        return HTTPReply(data: try encoder.encode(value), status: 200)
    }
}

func sampleSleep(zone: String = "Workshop", ends: String? = "2030-01-02T03:04:05Z", remaining: UInt64? = 1800,
                 fades: Bool = true, fading: Bool = false, done: String = "Timer running", changed: Bool = false) -> SleepTimer {
    SleepTimer(zone: zone, ends: ends, remainingSecs: remaining, fades: fades, fading: fading, done: done, changed: changed)
}
func sleepClient(_ transport: NativeSleepTransport) throws -> DaemonClient {
    DaemonClient(endpoint: try DaemonEndpoint("https://daemon.example.test"), transport: transport)
}
func waitForSleepRead(_ transport: NativeSleepTransport) async throws {
    let deadline = ContinuousClock.now.advanced(by: .seconds(3))
    while !(await transport.blocked) {
        guard ContinuousClock.now < deadline else { throw ContractError.invalid("The held sleep request never arrived") }
        try await Task.sleep(for: .milliseconds(5))
    }
}
@MainActor func sleepModel(_ transport: NativeSleepTransport) async throws -> RemoteModel {
    let model = try RemoteModel(client: sleepClient(transport))
    model.show(.sleep); model.resume()
    try await automationEventually { model.house?.isFresh == true && model.sleepObservedAt != nil && !model.panelLoading }
    return model
}

@Test func nativeSleepPayloadsMatchSetExtendAndCancelContracts() throws {
    let zone = automationZone(house: "TEST-B")
    let set = try NativeCommand.sleep(.set(seconds: 2700)).payload(in: zone)
    #expect(set.path == ["sleep"])
    #expect(set.body == ["zone": .string("Workshop@TEST-B"), "duration": .string("2700s")])
    let extend = try NativeCommand.sleep(.extend(seconds: 900)).payload(in: zone)
    #expect(extend.body == ["zone": .string("Workshop@TEST-B"), "duration": .string("900s"), "extend": .bool(true)])
    let cancel = try NativeCommand.sleep(.cancel).payload(in: zone)
    #expect(cancel.body == ["zone": .string("Workshop@TEST-B"), "cancel": .bool(true)])
    for seconds: UInt32 in [0, 82_801, .max] {
        #expect(throws: ContractError.self) { try NativeCommand.sleep(.set(seconds: seconds)).payload(in: zone) }
    }
    #expect(try NativeCommand.sleep(.set(seconds: 82_800)).payload(in: zone).body["duration"] == .string("82800s"))
}

@Test func nativeSleepMinutesNeverCoerceMalformedInputIntoAnAction() {
    for text in ["", " ", "0", "-1", "+1", "1e2", "1.5", "NaN", "∞", "٤٥", "1381", "999999999999999"] {
        #expect(SleepChange.durationSeconds(minutes: text) == nil)
    }
    #expect(SleepChange.durationSeconds(minutes: " 45 ") == 2700)
    #expect(SleepChange.durationSeconds(minutes: "1380") == 82_800)
}

@Test func nativeSleepReadsAreQualifiedAndAnEmptyListMeansNoTimer() async throws {
    let transport = NativeSleepTransport(), client = try sleepClient(transport)
    await transport.set(sampleSleep(), at: "Workshop@TEST-B")
    #expect(try await client.sleepTimer(automationZone()) == nil)
    #expect(try await client.sleepTimer(automationZone(house: "TEST-B"))?.fades == true)
    let request = try #require(await transport.requests.last)
    #expect(URLComponents(url: request.url!, resolvingAgainstBaseURL: false)?.queryItems?.first?.value == "Workshop@TEST-B")
    #expect(await transport.writes().isEmpty)
}

@Test func nativeSleepRejectsAmbiguousOrContradictoryWireReplies() async throws {
    let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
    let malformed: [[SleepTimer]] = [
        [sampleSleep(), sampleSleep()], [sampleSleep(zone: "Other room")], [sampleSleep(ends: "not-a-date")],
        [sampleSleep(remaining: nil)], [sampleSleep(ends: nil)], [sampleSleep(fades: false, fading: true)],
        [sampleSleep(ends: nil, remaining: nil)], [sampleSleep(done: " ")]
    ]
    for value in malformed {
        let transport = NativeSleepTransport()
        await transport.overrideRead(String(decoding: try encoder.encode(value), as: UTF8.self))
        await #expect(throws: ContractError.self) { try await sleepClient(transport).sleepTimer(automationZone()) }
    }
}

@Test func nativeSleepDeadlineComparisonIgnoresCountdownNotTimerReplacement() throws {
    let initial = sampleSleep()
    #expect(initial.sameTimer(as: sampleSleep(remaining: 1700, done: "Pauses soon")))
    #expect(initial.sameTimer(as: sampleSleep(ends: "2030-01-01T22:04:05-05:00")))
    #expect(!initial.sameTimer(as: sampleSleep(ends: "2030-01-02T03:05:05Z")))
    #expect(!initial.sameTimer(as: sampleSleep(fading: true)))
    let speaker = sampleSleep(ends: "2030-01-02T03:04:05.100Z", fades: false)
    #expect(speaker.sameTimer(as: sampleSleep(ends: "2030-01-02T03:04:05.800Z", remaining: 1799, fades: false)))
    #expect(!speaker.sameTimer(as: sampleSleep(ends: "2030-01-02T03:04:07.100Z", fades: false)))
    #expect(!initial.sameTimer(as: sampleSleep(ends: "2030-01-02T03:04:05.800Z")))
}

@Test func nativeSleepCountdownReachingZeroDoesNotBecomeACancellation() throws {
    let timer = sampleSleep(), end = try #require(timer.deadline)
    #expect(timer.estimatedRemaining(at: end.addingTimeInterval(-60)) == 60)
    #expect(timer.estimatedRemaining(at: end.addingTimeInterval(30)) == 0)
    #expect(timer.isRunning)
    #expect(sampleSleep(ends: nil, remaining: nil).estimatedRemaining(at: end) == nil)
}

@Test func nativeSleepSetReviewsCurrentTimerThenSendsOnlyOnce() async throws {
    let transport = NativeSleepTransport(), session = try CommandReviewSession(client: sleepClient(transport))
    let review = try await session.prepare(.sleep(.set(seconds: 2700)), in: automationZone())
    #expect(review.sleepTimer == nil)
    #expect(await transport.writes().isEmpty)
    let receipt = try await session.execute(review)
    #expect(receipt.detail.contains("Daemon fade enabled"))
    #expect(await transport.writes().count == 1)
    #expect(await transport.timerReads() == 2)
    await #expect(throws: ContractError.self) { try await session.execute(review) }
    #expect(await transport.writes().count == 1)
}

@Test func nativeSleepExtensionAllowsCountdownProgressAndPreservesTheDeadline() async throws {
    let transport = NativeSleepTransport()
    await transport.set(sampleSleep())
    let session = try CommandReviewSession(client: sleepClient(transport))
    let review = try await session.prepare(.sleep(.extend(seconds: 900)), in: automationZone())
    await transport.set(sampleSleep(remaining: 1780, done: "Timer counts down"))
    _ = try await session.execute(review)
    let timer = try #require(try await sleepClient(transport).sleepTimer(automationZone()))
    #expect(timer.deadline == sampleSleep().deadline?.addingTimeInterval(900))
    #expect(await transport.writes().count == 1)
}

@Test func nativeSleepExtensionRejectsAbsentTimerAndBoundsWithoutOverflow() async throws {
    let transport = NativeSleepTransport(), session = try CommandReviewSession(client: sleepClient(transport))
    await #expect(throws: ContractError.self) { try await session.prepare(.sleep(.extend(seconds: 60)), in: automationZone()) }
    for remaining: UInt64 in [82_799, .max] {
        await transport.set(sampleSleep(remaining: remaining))
        await #expect(throws: ContractError.self) { try await session.prepare(.sleep(.extend(seconds: 60)), in: automationZone()) }
    }
    #expect(await transport.writes().isEmpty)
}

@Test func nativeSleepCancelCanAcknowledgeAnIdempotentNoOpWithoutResumingMusic() async throws {
    let transport = NativeSleepTransport(), session = try CommandReviewSession(client: sleepClient(transport))
    let review = try await session.prepare(.sleep(.cancel), in: automationZone())
    let receipt = try await session.execute(review)
    #expect(receipt.detail.contains("does not resume"))
    #expect(await transport.writes().map { $0.url!.path } == ["/sleep"])
    #expect(try await sleepClient(transport).sleepTimer(automationZone()) == nil)
}

@Test func nativeSleepChangedDeadlineRefusesSetExtendAndCancel() async throws {
    for change: SleepChange in [.set(seconds: 900), .extend(seconds: 900), .cancel] {
        let transport = NativeSleepTransport()
        await transport.set(sampleSleep())
        let session = try CommandReviewSession(client: sleepClient(transport))
        let review = try await session.prepare(.sleep(change), in: automationZone())
        await transport.set(sampleSleep(ends: "2030-01-02T04:00:00Z"))
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes().isEmpty)
    }
}

@Test func nativeSleepFailedReadIsNotPermissionToReplaceAnUnknownTimer() async throws {
    let transport = NativeSleepTransport(), session = try CommandReviewSession(client: sleepClient(transport))
    await transport.failRead()
    await #expect(throws: URLError.self) { try await session.prepare(.sleep(.set(seconds: 900)), in: automationZone()) }
    #expect(await transport.writes().isEmpty)
}

@Test func nativeSleepUnrelatedMemberFailureDoesNotBlockCoordinatorTimer() async throws {
    let transport = NativeSleepTransport(), session = try CommandReviewSession(client: sleepClient(transport))
    await transport.house.fail("/zones/Hall@TEST-A/state")
    let review = try await session.prepare(.sleep(.set(seconds: 900)), in: automationZone())
    _ = try await session.execute(review)
    #expect(await transport.writes().count == 1)
}

@Test func nativeSleepCancellationDuringConfirmationReadPreventsAPost() async throws {
    let transport = NativeSleepTransport(), session = try CommandReviewSession(client: sleepClient(transport))
    let review = try await session.prepare(.sleep(.set(seconds: 900)), in: automationZone())
    await transport.hold("GET")
    let task = Task { try await session.execute(review) }
    try await waitForSleepRead(transport)
    task.cancel(); await session.cancel(); await transport.release()
    await #expect(throws: CancellationError.self) { try await task.value }
    #expect(await transport.writes().isEmpty)
}

@Test func nativeSleepLostAndContradictoryReceiptsNeverReplay() async throws {
    for body in [nil, "{}", "{\"done\":\"Applied\",\"changed\":true}",
                 "{\"zone\":\"Workshop\",\"done\":\"Cancelled\",\"changed\":true,\"fades\":false,\"fading\":false}"] as [String?] {
        let transport = NativeSleepTransport(), session = try CommandReviewSession(client: sleepClient(transport))
        let review = try await session.prepare(.sleep(.set(seconds: 900)), in: automationZone())
        if let body { await transport.overrideWrite(body) } else { await transport.loseWrite() }
        await #expect(throws: ClientError.self) { try await session.execute(review) }
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes().count == 1)
    }
}

@Test func nativeSleepSpeakerOnlyReceiptNeverClaimsAFade() async throws {
    let transport = NativeSleepTransport(), session = try CommandReviewSession(client: sleepClient(transport))
    await transport.speakerTimer()
    let review = try await session.prepare(.sleep(.set(seconds: 900)), in: automationZone())
    let receipt = try await session.execute(review)
    #expect(receipt.detail.contains("no fade"))
    #expect(!receipt.detail.contains("Daemon fade enabled"))
}

@Test @MainActor func nativeSleepScreenConnectsSetExtendAndCancelToTheRealCommandPath() async throws {
    let transport = NativeSleepTransport(), model = try await sleepModel(transport)
    defer { model.stop() }
    #expect(model.sleepTimer == nil && model.sleepObservedAt != nil)
    for change: SleepChange in [.set(seconds: 2700), .extend(seconds: 900), .cancel] {
        model.prepare(.sleep(change))
        try await automationEventually { model.review != nil }
        model.confirm(try #require(model.review?.id))
        try await automationEventually { model.actionPhase == .idle && !model.panelLoading && model.result?.kind == .confirmed }
    }
    #expect(model.sleepTimer == nil && model.sleepObservedAt != nil)
    #expect(await transport.writes().count == 3)
    #expect(model.result?.scene == nil)
}

@Test @MainActor func nativeSleepFailedPanelClearsObservationRatherThanClaimingNoTimer() async throws {
    let transport = NativeSleepTransport()
    await transport.set(sampleSleep())
    let model = try await sleepModel(transport)
    defer { model.stop() }
    await transport.failRead(); model.refreshPanel()
    try await automationEventually { model.panelProblem != nil && !model.panelLoading }
    #expect(model.sleepTimer == nil && model.sleepObservedAt == nil)
    #expect(model.canPrepare)
    await transport.failRead(false); model.refreshPanel()
    try await automationEventually { model.sleepTimer != nil && model.sleepObservedAt != nil }
}

@Test @MainActor func nativeSleepLateReadCannotCrossHouseholds() async throws {
    let transport = NativeSleepTransport()
    await transport.set(sampleSleep())
    let model = try await sleepModel(transport)
    defer { model.stop() }
    await transport.hold("GET"); model.refreshPanel()
    try await waitForSleepRead(transport)
    model.select(automationZone(house: "TEST-B").id)
    await transport.release()
    try await automationEventually { !model.panelLoading && model.sleepObservedAt != nil }
    #expect(model.sleepTimer == nil)
    #expect(model.selectedZone?.household == "TEST-B")
}

@Test @MainActor func nativeSleepLateAcknowledgementKeepsItsOriginalHousehold() async throws {
    let transport = NativeSleepTransport(), model = try await sleepModel(transport)
    defer { model.stop() }
    model.prepare(.sleep(.set(seconds: 900)))
    try await automationEventually { model.review != nil }
    await transport.hold("POST"); model.confirm(try #require(model.review?.id))
    try await waitForSleepRead(transport)
    model.select(automationZone(house: "TEST-B").id)
    await transport.release()
    try await automationEventually { model.actionPhase == .idle && model.result != nil }
    #expect(model.result?.kind == .confirmed && model.result?.target.household == "TEST-A")
    #expect(await transport.writes().count == 1)
}

@Test @MainActor func nativeSleepSuspensionFencesTheReviewAndDoesNotCreateALocalTimer() async throws {
    let transport = NativeSleepTransport(), model = try await sleepModel(transport)
    defer { model.stop() }
    model.prepare(.sleep(.set(seconds: 900)))
    try await automationEventually { model.review != nil }
    let id = try #require(model.review?.id)
    model.suspend(); model.confirm(id)
    #expect(model.sleepObservedAt == nil && model.review == nil)
    model.resume()
    try await automationEventually { model.sleepObservedAt != nil }
    #expect(await transport.writes().isEmpty)
}

@Test @MainActor func nativeSleepQuietPanelDoesNotPollOrReplayCommands() async throws {
    let transport = NativeSleepTransport()
    await transport.set(sampleSleep())
    let model = try await sleepModel(transport)
    defer { model.stop() }
    try await Task.sleep(for: .milliseconds(300))
    let reads = await transport.timerReads()
    try await Task.sleep(for: .milliseconds(300))
    #expect(await transport.timerReads() == reads)
    #expect(await transport.writes().isEmpty)
}
