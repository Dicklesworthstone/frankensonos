import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

private func nativePlaylistZone(_ household: String = "S2") -> Zone {
    Zone(household: household, coordinatorRoom: "Room + &", members: ["Room + &"], transportState: "playing")
}
private func nativePlaylist(_ title: String = "Evening", id: String = "SQ:7") -> SonosPlaylist {
    SonosPlaylist(id: id, title: title, artUri: nil)
}
private func nativePlaylistState(_ zone: Zone, uri: String = "fixture:current") -> ZoneState {
    ZoneState(zone: zone, transportState: "playing", volume: 20, mute: false, groupVolume: 20,
        track: Track(uri: uri, title: "Current", creator: nil, album: nil, durationSecs: 300,
                     positionSecs: 10, queuePosition: 1), shuffle: false, repeat: "off")
}
private func nativeContents(_ changes: [String: JSONValue] = [:]) throws -> Data {
    var body: [String: JSONValue] = ["zone": .string("Room + &"),
        "playlist": .object(["id": .string("SQ:7"), "title": .string("Evening")]),
        "revision": .string("saved:r1+&"), "total": .integer(3), "offset": .integer(0), "next_offset": .integer(2),
        "items": .array([
            .object(["position": .integer(1), "title": .string("Repeated"), "uri": .string("fixture:repeat")]),
            .object(["position": .integer(2), "title": .string("Repeated"), "uri": .string("fixture:repeat")])])]
    body.merge(changes) { _, new in new }
    return try JSONEncoder().encode(body)
}

private actor NativePlaylistsTransport: DaemonTransport {
    var requests: [URLRequest] = []
    var playlists = [nativePlaylist()]
    var revision = "queue-r1"
    var uri = "fixture:current"
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
    func replace(_ values: [SonosPlaylist]) { playlists = values }
    func changeQueue() { revision = "queue-r2" }
    func changeTrack() { uri = "fixture:next" }
    func failWrite(malformed: Bool = false) { loseWrite = !malformed; malformedWrite = malformed }
    func hold(_ path: String) { holdPath = path }
    func release() { holdPath = nil; waiting = false; waiter?.resume(); waiter = nil }

    func send(_ request: URLRequest) async throws -> HTTPReply {
        requests.append(request)
        let path = request.url!.path
        // Capture the old catalog before a deliberately late response.
        let observed = playlists
        if path == holdPath {
            holdPath = nil; waiting = true
            await withCheckedContinuation { waiter = $0 }
        }
        let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
        if request.httpMethod == "POST" {
            if loseWrite { throw URLError(.networkConnectionLost) }
            if malformedWrite { return HTTPReply(data: Data("{}".utf8), status: 200) }
            if path == "/sonos/playlists/play" {
                return HTTPReply(data: Data(#"{"done":"Playing saved playlist","changed":true}"#.utf8), status: 200)
            }
            guard path == "/queue" else { throw ContractError.invalid("Unexpected write: \(path)") }
            return HTTPReply(data: Data(#"{"done":"Saved playlist queued","zone":"Room + &","first":2,"count":3,"playing_queue":false}"#.utf8), status: 200)
        }
        if path == "/zones" {
            return HTTPReply(data: try encoder.encode([nativePlaylistZone("S2"), nativePlaylistZone("S1")]), status: 200)
        }
        if path.hasSuffix("/state") {
            let zone = nativePlaylistZone(path.contains("@S1/") ? "S1" : "S2")
            return HTTPReply(data: try encoder.encode(nativePlaylistState(zone, uri: uri)), status: 200)
        }
        if path.hasSuffix("/queue") {
            let body: [String: JSONValue] = ["zone": .string("Room + &"), "total": .integer(1),
                "revision": .string(revision), "playing": .integer(1),
                "items": .array([.object(["position": .integer(1), "title": .string("Current"), "uri": .string("fixture:current")])])]
            return HTTPReply(data: try JSONEncoder().encode(body), status: 200)
        }
        if path == "/sonos/playlists" { return HTTPReply(data: try encoder.encode(observed), status: 200) }
        if path == "/sonos/playlists/contents" {
            let query = URLComponents(url: request.url!, resolvingAgainstBaseURL: false)!.queryItems!
            if query.first(where: { $0.name == "offset" })?.value == "2" {
                return HTTPReply(data: try nativeContents(["offset": .integer(2), "next_offset": .null,
                    "items": .array([.object(["position": .integer(3), "title": .string("Unavailable")])])]), status: 200)
            }
            return HTTPReply(data: try nativeContents(), status: 200)
        }
        throw ContractError.invalid("Unexpected read: \(path)")
    }

    nonisolated func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> {
        output.yield(.connected); return stream
    }
}
private func nativePlaylistClient(_ transport: NativePlaylistsTransport) throws -> DaemonClient {
    DaemonClient(endpoint: try DaemonEndpoint("https://music.example.test"), transport: transport)
}
private func nativePlaylistWait(_ condition: @Sendable () async -> Bool) async throws {
    let deadline = ContinuousClock.now.advanced(by: .seconds(3))
    while !(await condition()) {
        guard ContinuousClock.now < deadline else { throw ContractError.invalid("Native playlist test did not reach its gate") }
        try await Task.sleep(for: .milliseconds(2))
    }
}

@Suite("Saved Sonos playlist native contract") struct SonosPlaylistsTests {
    @Test func catalogUsesTheQualifiedHouseholdAndOnlyReads() async throws {
        let transport = NativePlaylistsTransport(), client = try nativePlaylistClient(transport)
        let values = try await client.sonosPlaylists(nativePlaylistZone("S1"))
        #expect(values == [nativePlaylist()])
        let request = try #require(await transport.requests.first)
        #expect(request.url!.path == "/sonos/playlists")
        #expect(URLComponents(url: request.url!, resolvingAgainstBaseURL: false)?.queryItems == [
            URLQueryItem(name: "zone", value: "Room + &@S1")])
        #expect(request.url!.absoluteString.contains("%2B"))
        #expect(await transport.writes.isEmpty)
    }

    @Test func nativeIdentitiesAndDuplicateCatalogEntriesAreValidated() async throws {
        for id in ["", "SQ:", "SQ: 7", "SQ:7 ", "SQ:-1", "SQ:７", "FV:7", "file:///queue"] {
            #expect(throws: ClientError.self) { try nativePlaylist(id: id).validated() }
        }
        let transport = NativePlaylistsTransport(), client = try nativePlaylistClient(transport)
        await transport.replace([nativePlaylist(), nativePlaylist("Duplicate")])
        await #expect(throws: (any Error).self) { try await client.sonosPlaylists(nativePlaylistZone()) }
        await transport.replace([nativePlaylist(), nativePlaylist(id: "SQ:8")])
        #expect(try await client.sonosPlaylists(nativePlaylistZone()).count == 2)
    }

    @Test func boundedContentsPreserveDuplicatesMissingResourcesAndExactContinuation() async throws {
        let transport = NativePlaylistsTransport(), client = try nativePlaylistClient(transport)
        let request = try SonosPlaylistContentsRequest(playlist: nativePlaylist(), in: nativePlaylistZone())
        let page = try await client.sonosPlaylistContents(request)
        #expect(page.items.map(\.position) == [1, 2])
        #expect(page.items[0].uri == page.items[1].uri)
        let next = try SonosPlaylistContentsRequest(playlist: page.playlist, in: request.zone,
            offset: try #require(page.nextOffset), expectedRevision: page.revision)
        let last = try await client.sonosPlaylistContents(next)
        #expect(last.items.map(\.position) == [3] && last.items[0].uri == nil && last.nextOffset == nil)
        let sent = try #require(await transport.requests.last)
        #expect(URLComponents(url: sent.url!, resolvingAgainstBaseURL: false)?.queryItems == next.queryItems)
        #expect(await transport.writes.isEmpty)
    }

    @Test func continuationAndPageContradictionsNeverBecomePlayableChoices() throws {
        let zone = nativePlaylistZone(), playlist = nativePlaylist()
        #expect(throws: (any Error).self) { try SonosPlaylistContentsRequest(playlist: playlist, in: zone, offset: 1) }
        #expect(throws: (any Error).self) {
            try SonosPlaylistContentsRequest(playlist: playlist, in: zone, offset: UInt32.max, expectedRevision: "r")
        }
        let request = try SonosPlaylistContentsRequest(playlist: playlist, in: zone, expectedRevision: "saved:r1+&")
        for changes: [String: JSONValue] in [
            ["zone": .string("Other")], ["revision": .string("changed")], ["offset": .integer(2)],
            ["next_offset": .integer(1)], ["items": .array([])], ["total": .integer(1)],
            ["playlist": .object(["id": .string("SQ:8"), "title": .string("Evening")])],
            ["items": .array([.object(["position": .integer(2), "title": .string("Wrong position")])])]
        ] {
            #expect(throws: (any Error).self) {
                try DaemonClient.decode(SonosPlaylistContents.self, from: nativeContents(changes)).validated(for: request)
            }
        }
    }

    @Test func catalogTitlesAreComparedExactlyAndQueueReceiptsRequireEvidence() throws {
        let zone = nativePlaylistZone()
        let selection = try SonosPlaylistSelection(nativePlaylist("é"), in: zone, action: .append)
        #expect(throws: (any Error).self) { try selection.validate([nativePlaylist("e\u{301}")], in: zone) }
        for body in [
            #"{"done":"Queued","zone":"Other","first":1,"count":1,"playing_queue":false}"#,
            #"{"done":"Queued","zone":"Room + &","first":0,"count":1,"playing_queue":false}"#,
            #"{"done":"Queued","zone":"Room + &","first":1,"count":0,"playing_queue":false}"#,
            #"{"done":"Queued","zone":"Room + &","first":4294967295,"count":2,"playing_queue":false}"#,
            #"{"done":"Queued","zone":"Room + &","first":1,"count":1}"#
        ] {
            #expect(throws: (any Error).self) { try selection.receipt(Data(body.utf8), in: zone) }
        }
    }

    @Test(arguments: ["S1", "S2"], SonosPlaylistAction.allCases)
    func playbackActionsUseNativeIDsAndKeepQueueModesSilent(home: String, action: SonosPlaylistAction) throws {
        let zone = nativePlaylistZone(home)
        let selection = try SonosPlaylistSelection(nativePlaylist(), in: zone, action: action)
        let command = NativeCommand.sonosPlaylist(selection), payload = try command.payload(in: zone)
        #expect(payload.path == (action == .play ? ["sonos", "playlists", "play"] : ["queue"]))
        var expected: [String: JSONValue] = ["zone": .string(zone.id.address), "playlist": .string("SQ:7")]
        if action != .play { expected["next"] = .bool(action == .next) }
        #expect(payload.body == expected && payload.timeout == 600)
        #expect(command.needsSourceQueueReview && command.sourceQueueNeedsOccurrence == (action != .append))
        #expect(throws: (any Error).self) { try selection.fields(in: nativePlaylistZone(home == "S1" ? "S2" : "S1")) }
        let before = nativePlaylistState(zone), after = nativePlaylistState(zone, uri: "fixture:next")
        if action == .append { try command.validate(before: before, current: after) }
        else { #expect(throws: (any Error).self) { try command.validate(before: before, current: after) } }
    }

    @Test(arguments: SonosPlaylistAction.allCases)
    func reviewThenConfirmSendsOneRequestAndCannotReplay(action: SonosPlaylistAction) async throws {
        let transport = NativePlaylistsTransport(), zone = nativePlaylistZone()
        let session = CommandReviewSession(client: try nativePlaylistClient(transport))
        let review = try await session.prepare(.sonosPlaylist(try SonosPlaylistSelection(nativePlaylist(), in: zone, action: action)), in: zone)
        #expect(await transport.writes.isEmpty)
        let receipt = try await session.execute(review)
        #expect(!receipt.partial)
        if action != .play { #expect(receipt.detail.contains("current source was left unchanged")) }
        await #expect(throws: (any Error).self) { try await session.execute(review) }
        #expect(await transport.writes.count == 1)
        #expect(await transport.requests.filter { $0.url?.path == "/sonos/playlists" }.count == 2)
    }

    @Test func renamedMissingAmbiguousOrChangedDestinationRefusesBeforeDispatch() async throws {
        for changed in [[nativePlaylist("Renamed")], [], [nativePlaylist(), nativePlaylist()], [nativePlaylist(id: "SQ:8")]] {
            let transport = NativePlaylistsTransport(), zone = nativePlaylistZone()
            let session = CommandReviewSession(client: try nativePlaylistClient(transport))
            let command = NativeCommand.sonosPlaylist(try SonosPlaylistSelection(nativePlaylist(), in: zone, action: .play))
            let review = try await session.prepare(command, in: zone)
            await transport.replace(changed)
            await #expect(throws: (any Error).self) { try await session.execute(review) }
            #expect(await transport.writes.isEmpty)
        }
        let transport = NativePlaylistsTransport(), zone = nativePlaylistZone()
        let session = CommandReviewSession(client: try nativePlaylistClient(transport))
        let command = NativeCommand.sonosPlaylist(try SonosPlaylistSelection(nativePlaylist(), in: zone, action: .append))
        let review = try await session.prepare(command, in: zone)
        await transport.changeQueue()
        await #expect(throws: (any Error).self) { try await session.execute(review) }
        #expect(await transport.writes.isEmpty)
    }

    @Test func cancelledCatalogPreflightNeverSends() async throws {
        let transport = NativePlaylistsTransport(), zone = nativePlaylistZone()
        let session = CommandReviewSession(client: try nativePlaylistClient(transport))
        let review = try await session.prepare(.sonosPlaylist(try SonosPlaylistSelection(nativePlaylist(), in: zone, action: .next)), in: zone)
        await transport.hold("/sonos/playlists")
        let task = Task { try await session.execute(review) }
        try await nativePlaylistWait { await transport.waiting }
        await session.cancel(); await transport.release()
        await #expect(throws: CancellationError.self) { try await task.value }
        #expect(await transport.writes.isEmpty)
    }

    @Test(arguments: [false, true])
    func lostOrMalformedAcknowledgementStaysUncertainAndNeverRetries(malformed: Bool) async throws {
        let transport = NativePlaylistsTransport(), zone = nativePlaylistZone()
        let session = CommandReviewSession(client: try nativePlaylistClient(transport))
        let review = try await session.prepare(.sonosPlaylist(try SonosPlaylistSelection(nativePlaylist(), in: zone, action: .append)), in: zone)
        await transport.failWrite(malformed: malformed)
        do { _ = try await session.execute(review); Issue.record("Expected an uncertain write") }
        catch ClientError.uncertain { }
        await #expect(throws: (any Error).self) { try await session.execute(review) }
        #expect(await transport.writes.count == 1)
    }
}

@MainActor private func nativePlaylistScreenWait(_ condition: () -> Bool) async throws {
    let deadline = ContinuousClock.now.advanced(by: .seconds(3))
    while !condition() {
        guard ContinuousClock.now < deadline else { throw ContractError.invalid("Native playlist screen did not settle") }
        try await Task.sleep(for: .milliseconds(5))
    }
}

@Suite("Saved Sonos playlist native screen") struct SonosPlaylistScreenTests {
    @Test @MainActor func browsingAndPageChangesStayReadOnlyThenUseSharedReview() async throws {
        let transport = NativePlaylistsTransport(), model = RemoteModel(client: try nativePlaylistClient(transport))
        model.resume(); defer { model.stop(); transport.output.finish() }
        try await nativePlaylistScreenWait { model.selectedZone != nil && !model.panelLoading }
        model.show(.sonosPlaylists)
        try await nativePlaylistScreenWait { !model.panelLoading && !model.sonosPlaylists.isEmpty }
        let zone = try #require(model.selectedZone)
        model.openSonosPlaylist(nativePlaylist(), in: zone)
        try await nativePlaylistScreenWait { model.sonosPlaylistContents != nil && !model.panelLoading }
        model.sonosPlaylistPage(offset: 2)
        try await nativePlaylistScreenWait { model.sonosPlaylistContents?.offset == 2 && !model.panelLoading }
        #expect(await transport.writes.isEmpty)
        model.prepareSonosPlaylist(nativePlaylist(), in: zone, action: .append)
        try await nativePlaylistScreenWait { model.review != nil }
        let review = try #require(model.review)
        model.confirm(review.id); model.confirm(review.id)
        try await nativePlaylistScreenWait { model.result?.kind == .confirmed && model.actionPhase == .idle }
        #expect(await transport.writes.count == 1)
    }

    @Test @MainActor func lateCatalogCannotRepaintAnotherHousehold() async throws {
        let transport = NativePlaylistsTransport(), model = RemoteModel(client: try nativePlaylistClient(transport))
        model.resume(); defer { model.stop(); transport.output.finish() }
        try await nativePlaylistScreenWait { model.selectedZone != nil && !model.panelLoading }
        await transport.hold("/sonos/playlists")
        model.show(.sonosPlaylists)
        try await nativePlaylistWait { await transport.waiting }
        let before = try #require(model.selectedZone)
        let next = nativePlaylistZone(before.household == "S1" ? "S2" : "S1")
        model.select(next.id)
        await transport.replace([nativePlaylist("New household")]); await transport.release()
        try await nativePlaylistScreenWait { !model.panelLoading && !model.sonosPlaylists.isEmpty }
        #expect(model.sonosPlaylists.first?.title == "New household")
        model.prepareSonosPlaylist(nativePlaylist(), in: before, action: .play)
        #expect(model.review == nil && model.actionPhase == .idle)
        #expect(await transport.writes.isEmpty)
    }

    @Test @MainActor func lateContentsCannotReviveAClosedOrRetargetedPreview() async throws {
        for changeHousehold in [false, true] {
            let transport = NativePlaylistsTransport(), model = RemoteModel(client: try nativePlaylistClient(transport))
            model.resume()
            try await nativePlaylistScreenWait { model.selectedZone != nil && !model.panelLoading }
            model.show(.sonosPlaylists)
            try await nativePlaylistScreenWait { !model.panelLoading && !model.sonosPlaylists.isEmpty }
            let zone = try #require(model.selectedZone)
            await transport.hold("/sonos/playlists/contents")
            model.openSonosPlaylist(nativePlaylist(), in: zone)
            try await nativePlaylistWait { await transport.waiting }
            if changeHousehold { model.select(nativePlaylistZone(zone.household == "S1" ? "S2" : "S1").id) }
            else { model.closeSonosPlaylist() }
            await transport.release()
            try await nativePlaylistScreenWait { !model.panelLoading && !model.sonosPlaylists.isEmpty }
            #expect(model.sonosPlaylistRequest == nil && model.sonosPlaylistContents == nil)
            #expect(await transport.writes.isEmpty)
            model.stop(); transport.output.finish()
        }
    }
}
