import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

private func inputZone(_ household: String = "S1", members: [String] = ["Room + &"]) -> Zone {
    Zone(household: household, coordinatorRoom: "Room + &", members: members, transportState: "playing")
}
private func inputSource(_ player: String = "RINCON_PAIR_RIGHT", room: String = "Turntable",
                         name: String = "Record player", icon: String = "Phono") -> LineInSource {
    LineInSource(player: player, room: room, name: name, icon: icon)
}
private func inputCatalog() -> LineInCatalog {
    LineInCatalog(sources: [inputSource("RINCON_PAIR_LEFT"), inputSource()], unavailable: [
        LineInUnavailable(player: "RINCON_OFFLINE", room: "Hall", detail: "The player did not answer.")
    ])
}
private func inputState(_ zone: Zone, uri: String) -> ZoneState {
    ZoneState(zone: zone, transportState: "playing", volume: 20, mute: false, groupVolume: 20,
        track: Track(uri: uri, title: "Current", creator: nil, album: nil, durationSecs: 300,
                     positionSecs: 10, queuePosition: 1), shuffle: false, repeat: "off")
}

private actor LineInTransport: DaemonTransport {
    var requests: [URLRequest] = []
    var catalog = inputCatalog()
    var catalogBody: Data?
    var zones = [inputZone(), inputZone("S2")]
    var uri = "fixture:current"
    var changeDuringCatalog = false
    var loseWrite = false
    var malformedWrite = false
    var holdPath: String?
    var waiting = false
    private var waiter: CheckedContinuation<Void, Never>?
    nonisolated let stream: AsyncThrowingStream<EventSignal, Error>
    nonisolated let output: AsyncThrowingStream<EventSignal, Error>.Continuation

    init() {
        let pair = AsyncThrowingStream<EventSignal, Error>.makeStream()
        stream = pair.stream; output = pair.continuation
    }

    var writes: [URLRequest] { requests.filter { $0.httpMethod == "POST" } }
    func replace(_ value: LineInCatalog) { catalog = value }
    func malformedCatalog(_ value: String) { catalogBody = Data(value.utf8) }
    func regroup() { zones = [inputZone(members: ["Room + &", "Hall"]), inputZone("S2")] }
    func changeTrackDuringNextCatalog() { changeDuringCatalog = true }
    func failWrite(malformed: Bool) { loseWrite = !malformed; malformedWrite = malformed }
    func hold(_ path: String) { holdPath = path }
    func release() { holdPath = nil; waiting = false; waiter?.resume(); waiter = nil }

    func send(_ request: URLRequest) async throws -> HTTPReply {
        requests.append(request)
        let path = request.url!.path, observed = catalog, body = catalogBody
        if path == holdPath {
            holdPath = nil; waiting = true
            await withCheckedContinuation { waiter = $0 }
        }
        let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
        if request.httpMethod == "POST" {
            guard path == "/line-in/play" else { throw ContractError.invalid("Unexpected write: \(path)") }
            if loseWrite { throw URLError(.networkConnectionLost) }
            if malformedWrite { return HTTPReply(data: Data("{}".utf8), status: 200) }
            return HTTPReply(data: Data(#"{"done":"Playing the selected input","changed":true}"#.utf8), status: 200)
        }
        if path == "/line-in" {
            if changeDuringCatalog { uri = "fixture:new-owner"; changeDuringCatalog = false }
            return HTTPReply(data: try body ?? encoder.encode(observed), status: 200)
        }
        if path == "/zones" { return HTTPReply(data: try encoder.encode(zones), status: 200) }
        if path.hasSuffix("/state") {
            let zone = zones.first { $0.household == (path.contains("@S2/") ? "S2" : "S1") }!
            return HTTPReply(data: try encoder.encode(inputState(zone, uri: uri)), status: 200)
        }
        if path.hasSuffix("/queue") {
            return HTTPReply(data: Data(#"{"zone":"Room + &","total":1,"revision":"q1","playing":1,"items":[{"position":1,"title":"Current","uri":"fixture:current"}]}"#.utf8), status: 200)
        }
        throw ContractError.invalid("Unexpected read: \(path)")
    }

    nonisolated func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> {
        output.yield(.connected); return stream
    }
}
private func inputClient(_ transport: LineInTransport) throws -> DaemonClient {
    DaemonClient(endpoint: try DaemonEndpoint("https://music.example.test"), transport: transport)
}
private func inputWait(_ condition: @Sendable () async -> Bool) async throws {
    let deadline = ContinuousClock.now.advanced(by: .seconds(3))
    while !(await condition()) {
        guard ContinuousClock.now < deadline else { throw ContractError.invalid("Line-in test did not reach its gate") }
        try await Task.sleep(for: .milliseconds(2))
    }
}

@Suite("Native line-in contract") struct LineInTests {
    @Test func catalogIsHouseholdBoundReadOnlyAndKeepsBothPhysicalInputs() async throws {
        let transport = LineInTransport(), client = try inputClient(transport)
        let catalog = try await client.lineInSources(inputZone("S2"))
        #expect(catalog == inputCatalog())
        #expect(catalog.sources.map(\.player) == ["RINCON_PAIR_LEFT", "RINCON_PAIR_RIGHT"])
        #expect(catalog.unavailable.first?.room == "Hall")
        let request = try #require(await transport.requests.first)
        #expect(request.url!.path == "/line-in")
        #expect(URLComponents(url: request.url!, resolvingAgainstBaseURL: false)?.queryItems == [
            URLQueryItem(name: "zone", value: "Room + &@S2")])
        #expect(request.url!.absoluteString.contains("%2B"))
        #expect(await transport.writes.isEmpty)
        let encoded = try JSONSerialization.jsonObject(with: JSONEncoder().encode(catalog)) as! [String: Any]
        let sources = try #require(encoded["sources"] as? [[String: Any]])
        #expect(sources.allSatisfy { $0["connected"] == nil })
    }

    @Test func malformedOrAmbiguousCatalogIsNeverTreatedAsAnEmptyHousehold() async throws {
        for id in ["", " RINCON_INPUT", "RINCON_INPUT ", "RINCON_INPUT\n", "input#0", "input?x", "源", String(repeating: "x", count: 129)] {
            #expect(throws: ClientError.self) { try inputSource(id).validated() }
        }
        let unavailable = LineInUnavailable(player: "RINCON_PAIR_RIGHT", room: "Turntable", detail: "Timeout")
        for catalog in [
            LineInCatalog(sources: [inputSource(), inputSource()], unavailable: []),
            LineInCatalog(sources: [inputSource(), inputSource("rincon_pair_right")], unavailable: []),
            LineInCatalog(sources: [inputSource()], unavailable: [unavailable]),
            LineInCatalog(sources: [], unavailable: [unavailable, unavailable])
        ] {
            #expect(throws: ClientError.self) { try catalog.validated() }
        }
        for body in [#"{}"#, #"{"sources":[]}"#, #"{"unavailable":[]}"#,
                     #"{"sources":[{"player":"RINCON_INPUT","room":"Turntable","name":"Record player"}],"unavailable":[]}"#] {
            let transport = LineInTransport(), client = try inputClient(transport)
            await transport.malformedCatalog(body)
            await #expect(throws: (any Error).self) { try await client.lineInSources(inputZone()) }
            #expect(await transport.writes.isEmpty)
        }
        #expect(try inputSource(name: "").validated().displayName == "Line-in")
        #expect(try LineInCatalog(sources: [], unavailable: []).validated().sources.isEmpty)
    }

    @Test(arguments: ["S1", "S2"])
    func playPayloadRetainsTheExactSecondaryAndHasNoQueueMode(home: String) throws {
        let zone = inputZone(home), selection = try LineInSelection(inputSource(), in: zone)
        let command = NativeCommand.lineIn(selection), payload = try command.payload(in: zone)
        #expect(payload.path == ["line-in", "play"])
        #expect(payload.body == ["zone": .string(zone.id.address), "source": .string("RINCON_PAIR_RIGHT")])
        #expect(!command.isQueueAdd && !command.needsSourceQueueReview)
        #expect(command.explanation.contains("Cable connection and audio signal are unknown"))
        #expect(throws: (any Error).self) { try selection.fields(in: inputZone(home == "S1" ? "S2" : "S1")) }
        #expect(throws: (any Error).self) { try selection.fields(in: inputZone(home, members: ["Room + &", "Hall"])) }
    }

    @Test func reviewAndConfirmationRecheckTheInputAndSendExactlyOnce() async throws {
        let transport = LineInTransport(), zone = inputZone()
        let session = CommandReviewSession(client: try inputClient(transport))
        let command = NativeCommand.lineIn(try LineInSelection(inputSource(), in: zone))
        let review = try await session.prepare(command, in: zone)
        #expect(await transport.writes.isEmpty)
        let receipt = try await session.execute(review)
        #expect(receipt.title == "Playing the selected input" && !receipt.partial)
        await #expect(throws: (any Error).self) { try await session.execute(review) }
        let writes = await transport.writes
        #expect(writes.count == 1)
        let sent = try #require(writes.first)
        #expect(try JSONDecoder().decode([String: JSONValue].self, from: #require(sent.httpBody)) == [
            "zone": .string(zone.id.address), "source": .string("RINCON_PAIR_RIGHT")])
        let requests = await transport.requests
        #expect(requests.filter { $0.url?.path == "/line-in" }.count == 2)
        #expect(!requests.contains { $0.url!.path.hasSuffix("/queue") })
    }

    @Test func changedMissingUnreadableOrAmbiguousInputRefusesBeforeDispatch() async throws {
        let unavailable = LineInUnavailable(player: "RINCON_PAIR_RIGHT", room: "Turntable", detail: "Offline")
        for changed in [
            LineInCatalog(sources: [inputSource(name: "Renamed")], unavailable: []),
            LineInCatalog(sources: [inputSource(room: "Another room")], unavailable: []),
            LineInCatalog(sources: [inputSource(icon: "AudioComponent")], unavailable: []),
            LineInCatalog(sources: [inputSource("RINCON_PAIR_LEFT")], unavailable: []),
            LineInCatalog(sources: [], unavailable: []),
            LineInCatalog(sources: [], unavailable: [unavailable]),
            LineInCatalog(sources: [inputSource(), inputSource()], unavailable: [])
        ] {
            let transport = LineInTransport(), zone = inputZone()
            let session = CommandReviewSession(client: try inputClient(transport))
            let review = try await session.prepare(.lineIn(try LineInSelection(inputSource(), in: zone)), in: zone)
            await transport.replace(changed)
            await #expect(throws: (any Error).self) { try await session.execute(review) }
            #expect(await transport.writes.isEmpty)
        }
        let exact = try LineInSelection(inputSource(name: "é"), in: inputZone())
        #expect(throws: (any Error).self) {
            try exact.validate(LineInCatalog(sources: [inputSource(name: "e\u{301}")], unavailable: []), in: inputZone())
        }
    }

    @Test(arguments: [false, true])
    func changedGroupOrPlaybackDuringCatalogReadPreventsTheWrite(regroup: Bool) async throws {
        let transport = LineInTransport(), zone = inputZone()
        let session = CommandReviewSession(client: try inputClient(transport))
        let review = try await session.prepare(.lineIn(try LineInSelection(inputSource(), in: zone)), in: zone)
        if regroup { await transport.regroup() }
        else { await transport.changeTrackDuringNextCatalog() }
        await #expect(throws: (any Error).self) { try await session.execute(review) }
        #expect(await transport.writes.isEmpty)
    }

    @Test func cancellationWhileInputMetadataIsReadNeverDispatches() async throws {
        let transport = LineInTransport(), zone = inputZone()
        let session = CommandReviewSession(client: try inputClient(transport))
        let review = try await session.prepare(.lineIn(try LineInSelection(inputSource(), in: zone)), in: zone)
        await transport.hold("/line-in")
        let task = Task { try await session.execute(review) }
        try await inputWait { await transport.waiting }
        await session.cancel(); await transport.release()
        await #expect(throws: CancellationError.self) { try await task.value }
        #expect(await transport.writes.isEmpty)
    }

    @Test(arguments: [false, true])
    func uncertainAcknowledgementCannotReplayTheConsumedReview(malformed: Bool) async throws {
        let transport = LineInTransport(), zone = inputZone()
        let session = CommandReviewSession(client: try inputClient(transport))
        let review = try await session.prepare(.lineIn(try LineInSelection(inputSource(), in: zone)), in: zone)
        await transport.failWrite(malformed: malformed)
        do { _ = try await session.execute(review); Issue.record("Expected uncertain playback") }
        catch ClientError.uncertain { }
        await #expect(throws: (any Error).self) { try await session.execute(review) }
        #expect(await transport.writes.count == 1)
    }
}

@MainActor private func inputScreenWait(_ condition: () -> Bool) async throws {
    let deadline = ContinuousClock.now.advanced(by: .seconds(3))
    while !condition() {
        guard ContinuousClock.now < deadline else { throw ContractError.invalid("Line-in screen did not settle") }
        try await Task.sleep(for: .milliseconds(5))
    }
}

@Suite("Native line-in screen lifecycle") struct LineInScreenTests {
    @Test @MainActor func browsingShowsPartialReadsAndPlaybackUsesTheSharedReview() async throws {
        let transport = LineInTransport(), model = RemoteModel(client: try inputClient(transport))
        model.resume(); defer { model.stop(); transport.output.finish() }
        try await inputScreenWait { model.selectedZone != nil && !model.panelLoading }
        model.show(.lineIn)
        try await inputScreenWait { model.lineInCatalog != nil && !model.panelLoading }
        #expect(model.lineInCatalog?.sources.count == 2 && model.lineInCatalog?.unavailable.count == 1)
        #expect(await transport.writes.isEmpty)
        let zone = try #require(model.selectedZone)
        model.prepareLineIn(inputSource(), in: zone)
        try await inputScreenWait { model.review != nil }
        let review = try #require(model.review)
        model.confirm(review.id); model.confirm(review.id)
        try await inputScreenWait { model.result?.kind == .confirmed && model.actionPhase == .idle }
        #expect(await transport.writes.count == 1)
    }

    @Test @MainActor func lateInputCatalogCannotRepaintOrTargetAnotherHousehold() async throws {
        let transport = LineInTransport(), model = RemoteModel(client: try inputClient(transport))
        model.resume(); defer { model.stop(); transport.output.finish() }
        try await inputScreenWait { model.selectedZone != nil && !model.panelLoading }
        await transport.hold("/line-in"); model.show(.lineIn)
        try await inputWait { await transport.waiting }
        let before = try #require(model.selectedZone)
        let next = inputZone(before.household == "S1" ? "S2" : "S1")
        model.select(next.id)
        let source = inputSource("RINCON_NEW_HOUSEHOLD", name: "New household input")
        await transport.replace(LineInCatalog(sources: [source], unavailable: [])); await transport.release()
        try await inputScreenWait { !model.panelLoading && model.lineInCatalog != nil }
        #expect(model.lineInCatalog?.sources == [source])
        model.prepareLineIn(inputSource(), in: before)
        model.prepareLineIn(inputSource(), in: next)
        #expect(model.review == nil && model.actionPhase == .idle)
        #expect(await transport.writes.isEmpty)
    }

    @Test(arguments: [false, true]) @MainActor
    func suspensionOrLeavingThePanelDiscardsLateInputReplies(suspend: Bool) async throws {
        let transport = LineInTransport(), model = RemoteModel(client: try inputClient(transport))
        model.resume(); defer { model.stop(); transport.output.finish() }
        try await inputScreenWait { model.selectedZone != nil && !model.panelLoading }
        await transport.hold("/line-in"); model.show(.lineIn)
        try await inputWait { await transport.waiting }
        if suspend { model.suspend() } else { model.show(.queue) }
        await transport.release()
        try await inputScreenWait { !model.panelLoading }
        #expect(model.lineInCatalog == nil && model.review == nil)
        #expect(await transport.writes.isEmpty)
    }

    @Test @MainActor func changingTheListeningHouseholdInvalidatesAnUnsentReview() async throws {
        let transport = LineInTransport(), model = RemoteModel(client: try inputClient(transport))
        model.resume(); defer { model.stop(); transport.output.finish() }
        try await inputScreenWait { model.selectedZone != nil && !model.panelLoading }
        model.show(.lineIn)
        try await inputScreenWait { model.lineInCatalog != nil && !model.panelLoading }
        let zone = try #require(model.selectedZone)
        model.prepareLineIn(inputSource(), in: zone)
        try await inputScreenWait { model.review != nil }
        let review = try #require(model.review)
        model.select(inputZone(zone.household == "S1" ? "S2" : "S1").id)
        model.confirm(review.id)
        #expect(model.review == nil && model.lineInCatalog == nil)
        #expect(await transport.writes.isEmpty)
    }
}
