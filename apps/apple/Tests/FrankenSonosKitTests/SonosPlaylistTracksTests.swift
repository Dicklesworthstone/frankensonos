import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

private func tracksZone(_ home: String = "S2") -> Zone {
    Zone(household: home, coordinatorRoom: "Room + &", members: ["Room + &"], transportState: "playing")
}
private func tracksPlaylist() -> SonosPlaylist { SonosPlaylist(id: "SQ:7", title: "Ordered selections", artUri: nil) }
private func tracksRequest(_ offset: UInt32 = 0, home: String = "S2", revision: String? = nil) throws -> SonosPlaylistContentsRequest {
    try SonosPlaylistContentsRequest(playlist: tracksPlaylist(), in: tracksZone(home), offset: offset,
        expectedRevision: revision ?? (offset == 0 ? nil : "native:r1"))
}
private func tracksPageData(offset: UInt32 = 0, revision: String = "native:r1",
                            replaced: UInt32? = nil, unavailable: UInt32? = nil) throws -> Data {
    let count = min(UInt32(50), 60 - offset)
    let items: [JSONValue] = (0..<count).map { index in
        let position = offset + index + 1
        var item: [String: JSONValue] = [
            "position": .integer(Int64(position)), "title": .string("Track \(position)"),
            "creator": .string("Fixture artist"), "album": .string("Fixture album"), "duration_secs": .integer(180)
        ]
        if position != unavailable { item["uri"] = .string(position == replaced ? "fixture:replaced" : "fixture:track\(position)") }
        return .object(item)
    }
    return try JSONEncoder().encode([
        "zone": JSONValue.string("Room + &"),
        "playlist": .object(["id": .string("SQ:7"), "title": .string("Ordered selections")]),
        "revision": .string(revision), "total": .integer(60), "offset": .integer(Int64(offset)),
        "next_offset": offset + count < 60 ? .integer(Int64(offset + count)) : .null, "items": .array(items)
    ])
}
private func tracksPage(_ offset: UInt32 = 0, revision: String = "native:r1") throws -> SonosPlaylistContents {
    try DaemonClient.decode(SonosPlaylistContents.self, from: tracksPageData(offset: offset, revision: revision))
}
private func tracksDraft(home: String = "S2") throws -> SonosPlaylistTracksDraft {
    var draft = SonosPlaylistTracksDraft()
    let first = try tracksPage(), later = try tracksPage(50)
    try draft.append(first.items[1], from: first, request: tracksRequest(home: home))
    try draft.append(later.items[0], from: later, request: tracksRequest(50, home: home))
    try draft.append(first.items[1], from: first, request: tracksRequest(home: home))
    return draft
}

private actor TracksTransport: DaemonTransport {
    var requests: [URLRequest] = []
    var contentsOffsets: [UInt32] = []
    var savedRevision = "native:r1", queueRevision = "queue:r1"
    var currentTrack: UInt32 = 1
    var replaced: UInt32?
    var receiptError: String?
    var loseReply = false
    var blocked = false
    private var heldOffset: UInt32?
    private var heldWrite = false
    private var gate: CheckedContinuation<Void, Never>?
    nonisolated let stream: AsyncThrowingStream<EventSignal, Error>
    nonisolated let output: AsyncThrowingStream<EventSignal, Error>.Continuation

    init() {
        let pair = AsyncThrowingStream<EventSignal, Error>.makeStream()
        stream = pair.stream; output = pair.continuation
    }
    var writes: [URLRequest] { requests.filter { $0.httpMethod == "POST" } }
    func changeSavedRevision() { savedRevision = "native:r2" }
    func replaceLaterOccurrence() { replaced = 51 }
    func changeQueue() { queueRevision = "queue:r2" }
    func advance() { currentTrack = 2 }
    func failWrite(_ field: String? = nil) { receiptError = field; loseReply = field == nil }
    func holdPage(_ offset: UInt32) { heldOffset = offset }
    func holdWrite() { heldWrite = true }
    func release() { blocked = false; gate?.resume(); gate = nil }

    func send(_ request: URLRequest) async throws -> HTTPReply {
        requests.append(request)
        let path = request.url!.path.removingPercentEncoding ?? request.url!.path
        let query = URLComponents(url: request.url!, resolvingAgainstBaseURL: false)?.queryItems ?? []
        let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
        if request.httpMethod == "POST" {
            #expect(path == "/sonos/playlists/tracks")
            if heldWrite {
                heldWrite = false; blocked = true
                await withCheckedContinuation { gate = $0 }
            }
            if loseReply { throw URLError(.networkConnectionLost) }
            let body = try JSONDecoder().decode([String: JSONValue].self, from: request.httpBody!)
            guard case .array(let selected)? = body["positions"], case .string(let mode)? = body["mode"] else {
                throw ContractError.invalid("Missing native selected occurrences")
            }
            let positions = selected.compactMap { value -> UInt32? in
                guard case .integer(let number) = value else { return nil }
                return UInt32(exactly: number)
            }
            var receipt: [String: JSONValue] = [
                "done": .string("Selected native tracks accepted"), "zone": .string("Room + &"),
                "playlist": .string("SQ:7"), "positions": .array(selected),
                "titles": .array(positions.map { .string("Track \($0)") }), "mode": .string(mode),
                "changed": .bool(true), "notes": .array([])
            ]
            if mode != "play" {
                receipt["first"] = .integer(3); receipt["count"] = .integer(Int64(positions.count))
                receipt["playing_queue"] = .bool(false)
            }
            switch receiptError {
            case "positions": receipt["positions"] = .array(Array(selected.reversed()))
            case "title": receipt["titles"] = .array(positions.map { _ in .string("Different title") })
            case "playlist": receipt["playlist"] = .string("SQ:8")
            case "mode": receipt["mode"] = .string("unknown")
            case "zone": receipt["zone"] = .string("Another room")
            case "count": receipt["count"] = .integer(1)
            case "notes": receipt["notes"] = .null
            case "changed": receipt["changed"] = .bool(false)
            case "skipped":
                receipt["changed"] = .bool(false); receipt["done"] = .string("Selected playback was skipped")
                receipt["notes"] = .array([.object(["code": .string("SKIPPED"), "detail": .string("The selection was not played.")])])
            default: break
            }
            return HTTPReply(data: try JSONEncoder().encode(receipt), status: 200)
        }
        if path == "/zones" { return HTTPReply(data: try encoder.encode([tracksZone(), tracksZone("S1")]), status: 200) }
        if path.hasSuffix("/state") {
            let zone = tracksZone(path.contains("@S1/") ? "S1" : "S2")
            let state = ZoneState(zone: zone, transportState: "playing", volume: 20, mute: false, groupVolume: 20,
                track: Track(uri: "fixture:track\(currentTrack)", title: "Current", creator: nil, album: nil,
                    durationSecs: 180, positionSecs: 10, queuePosition: currentTrack), shuffle: false, repeat: "off")
            return HTTPReply(data: try encoder.encode(state), status: 200)
        }
        if path.hasSuffix("/queue") {
            let limit = Int(query.first(where: { $0.name == "limit" })?.value ?? "25") ?? 25
            let items: [JSONValue] = (1...min(limit, 2)).map { .object([
                "position": .integer(Int64($0)), "title": .string("Queue \($0)"), "uri": .string("fixture:queue\($0)")
            ]) }
            return HTTPReply(data: try JSONEncoder().encode([
                "zone": JSONValue.string("Room + &"), "total": .integer(2), "revision": .string(queueRevision),
                "playing": .integer(Int64(currentTrack)), "items": .array(items)
            ]), status: 200)
        }
        if path == "/sonos/playlists" { return HTTPReply(data: try encoder.encode([tracksPlaylist()]), status: 200) }
        if path == "/sonos/playlists/contents" {
            let offset = UInt32(query.first(where: { $0.name == "offset" })?.value ?? "")!
            #expect(query.contains(URLQueryItem(name: "limit", value: "50")))
            contentsOffsets.append(offset)
            if heldOffset == offset {
                heldOffset = nil; blocked = true
                await withCheckedContinuation { gate = $0 }
            }
            return HTTPReply(data: try tracksPageData(offset: offset, revision: savedRevision, replaced: replaced), status: 200)
        }
        throw ContractError.invalid("Unexpected native selection route: \(path)")
    }
    nonisolated func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> {
        output.yield(.connected); return stream
    }
}
private func tracksClient(_ transport: TracksTransport) throws -> DaemonClient {
    DaemonClient(endpoint: try DaemonEndpoint("https://music.example.test"), transport: transport)
}
private func tracksWait(_ condition: @Sendable () async -> Bool) async throws {
    let deadline = ContinuousClock.now.advanced(by: .seconds(3))
    while !(await condition()) {
        guard ContinuousClock.now < deadline else { throw ContractError.invalid("Native selection did not reach its test boundary") }
        try await Task.sleep(for: .milliseconds(2))
    }
}
@MainActor private func tracksModelWait(_ condition: () -> Bool) async throws {
    let deadline = ContinuousClock.now.advanced(by: .seconds(3))
    while !condition() {
        guard ContinuousClock.now < deadline else { throw ContractError.invalid("Native selection view did not reach its test boundary") }
        try await Task.sleep(for: .milliseconds(2))
    }
}

@Suite("Native saved-playlist ordered selection") struct SonosPlaylistTracksTests {
    @Test func selectedOrderAndDuplicateOccurrencesSurvivePagingAndEditing() throws {
        var draft = try tracksDraft()
        #expect(draft.choices.map(\.item.position) == [2, 51, 2])
        #expect(Set(draft.choices.map(\.id)).count == 3)
        let previous = draft.id
        try draft.move(draft.choices[1].id, by: -1)
        #expect(draft.choices.map(\.item.position) == [51, 2, 2])
        #expect(draft.id != previous)
        try draft.repeatChoice(draft.choices[0].id)
        #expect(draft.choices.map(\.item.position) == [51, 2, 2, 51])
        try draft.remove(draft.choices[1].id)
        #expect(draft.choices.map(\.item.position) == [51, 2, 51])
        #expect(throws: ContractError.self) { try draft.move(draft.choices[0].id, by: -1) }
        #expect(throws: ContractError.self) { try draft.remove(UUID()) }
    }

    @Test func boundsAndUnavailableOccurrencesNeverSilentlyChangeASelection() throws {
        var draft = try tracksDraft()
        while draft.choices.count < SonosPlaylistTracksDraft.maximumCount { try draft.repeatChoice(draft.choices[0].id) }
        #expect(throws: ContractError.self) { try draft.repeatChoice(draft.choices[0].id) }
        #expect(draft.choices.count == 10)
        let page = try DaemonClient.decode(SonosPlaylistContents.self, from: tracksPageData(unavailable: 1))
        var fresh = SonosPlaylistTracksDraft()
        #expect(throws: ContractError.self) { try fresh.append(page.items[0], from: page, request: tracksRequest()) }
        #expect(fresh.isEmpty)
        #expect(throws: ContractError.self) { try SonosPlaylistTracksSelection(fresh, action: .play) }
    }

    @Test func changedRevisionHouseholdOrOccurrenceRetiresOldChoices() throws {
        var draft = try tracksDraft()
        try draft.observe(tracksPage(revision: "native:r2"), request: tracksRequest())
        #expect(draft.isEmpty)
        draft = try tracksDraft()
        try draft.observe(tracksPage(), request: tracksRequest(home: "S1"))
        #expect(draft.isEmpty && draft.zone?.household == "S1")
        draft = try tracksDraft()
        let replaced = try DaemonClient.decode(SonosPlaylistContents.self, from: tracksPageData(offset: 50, replaced: 51))
        #expect(throws: ContractError.self) { try draft.observe(replaced, request: tracksRequest(50)) }
        #expect(draft.isEmpty)
    }

    @Test(arguments: ["S1", "S2"], [SonosPlaylistAction.play, .next, .append])
    func allModesReadEverySelectedPageTwiceThenUseOneNativeCommand(home: String, action: SonosPlaylistAction) async throws {
        let transport = TracksTransport(), session = CommandReviewSession(client: try tracksClient(transport))
        let selection = try SonosPlaylistTracksSelection(tracksDraft(home: home), action: action)
        let review = try await session.prepare(.sonosPlaylistTracks(selection), in: tracksZone(home))
        #expect(await transport.writes.isEmpty)
        #expect(await transport.contentsOffsets == [0, 50])
        let receipt = try await session.execute(review)
        #expect(!receipt.partial)
        #expect(await transport.contentsOffsets == [0, 50, 0, 50])
        let requests = await transport.writes
        #expect(requests.count == 1)
        let sent = try #require(requests.first)
        #expect(sent.url?.path == "/sonos/playlists/tracks")
        #expect(sent.timeoutInterval == 600)
        let body = try JSONDecoder().decode([String: JSONValue].self, from: #require(sent.httpBody))
        #expect(body == [
            "zone": .string("Room + &@\(home)"), "playlist": .string("SQ:7"),
            "positions": .array([.integer(2), .integer(51), .integer(2)]),
            "mode": .string(action.rawValue), "expected_revision": .string("native:r1"),
            "expected_queue_revision": .string("queue:r1")
        ])
        #expect(body["uri"] == nil && body["metadata"] == nil && body["source_uri"] == nil)
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes.count == 1)
    }

    @Test(arguments: ["saved", "queue", "occurrence"])
    func changedSourceOrDestinationStopsTheWholeSelection(change: String) async throws {
        let transport = TracksTransport(), session = CommandReviewSession(client: try tracksClient(transport))
        let selection = try SonosPlaylistTracksSelection(tracksDraft(), action: .next)
        let review = try await session.prepare(.sonosPlaylistTracks(selection), in: tracksZone())
        switch change {
        case "saved": await transport.changeSavedRevision()
        case "queue": await transport.changeQueue()
        default: await transport.replaceLaterOccurrence()
        }
        await #expect(throws: (any Error).self) { try await session.execute(review) }
        #expect(await transport.writes.isEmpty)
    }

    @Test(arguments: [SonosPlaylistAction.play, .next, .append])
    func onlyAppendAllowsNaturalTrackProgress(action: SonosPlaylistAction) async throws {
        let transport = TracksTransport(), session = CommandReviewSession(client: try tracksClient(transport))
        let selection = try SonosPlaylistTracksSelection(tracksDraft(), action: action)
        let review = try await session.prepare(.sonosPlaylistTracks(selection), in: tracksZone())
        await transport.advance()
        if action == .append { _ = try await session.execute(review); #expect(await transport.writes.count == 1) }
        else {
            await #expect(throws: ContractError.self) { try await session.execute(review) }
            #expect(await transport.writes.isEmpty)
        }
    }

    @Test func wrongHouseholdAndSingleTrackSelectionKeepTheirExactScope() throws {
        let selection = try SonosPlaylistTracksSelection(tracksDraft(), action: .play)
        #expect(throws: ContractError.self) { try NativeCommand.sonosPlaylistTracks(selection).payload(in: tracksZone("S1")) }
        let page = try tracksPage()
        var draft = SonosPlaylistTracksDraft()
        try draft.append(page.items[4], from: page, request: tracksRequest())
        let single = try SonosPlaylistTracksSelection(draft, action: .append)
        #expect(single.positions == [5])
        #expect(try single.fields(in: tracksZone())["positions"] == .array([.integer(5)]))
    }

    @Test func cancellationDuringALaterPageCannotFreeTheWriterOrDispatch() async throws {
        let transport = TracksTransport(), session = CommandReviewSession(client: try tracksClient(transport))
        let selection = try SonosPlaylistTracksSelection(tracksDraft(), action: .play)
        let review = try await session.prepare(.sonosPlaylistTracks(selection), in: tracksZone())
        await transport.holdPage(50)
        let task = Task { try await session.execute(review) }
        try await tracksWait { await transport.blocked }
        task.cancel(); await session.cancel()
        await #expect(throws: ContractError.self) { try await session.prepare(.pause, in: tracksZone()) }
        await transport.release()
        await #expect(throws: CancellationError.self) { try await task.value }
        #expect(await transport.writes.isEmpty)
    }

    @Test(arguments: ["lost", "positions", "title", "playlist", "mode", "zone", "count", "notes"])
    func lostOrMismatchedReceiptsStayUncertainAndAreNeverRepeated(field: String) async throws {
        let transport = TracksTransport(), session = CommandReviewSession(client: try tracksClient(transport))
        var draft = try tracksDraft()
        try draft.remove(draft.choices[2].id) // Non-palindromic order makes a reversed receipt detectably wrong.
        let selection = try SonosPlaylistTracksSelection(draft, action: .append)
        let review = try await session.prepare(.sonosPlaylistTracks(selection), in: tracksZone())
        await transport.failWrite(field == "lost" ? nil : field)
        do { _ = try await session.execute(review); Issue.record("Malformed selected-track receipt accepted") }
        catch ClientError.uncertain { }
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes.count == 1)
    }

    @Test(arguments: [SonosPlaylistAction.next, .append])
    func queueAddReceiptsMustAcknowledgeAChange(action: SonosPlaylistAction) async throws {
        let transport = TracksTransport(), session = CommandReviewSession(client: try tracksClient(transport))
        let selection = try SonosPlaylistTracksSelection(tracksDraft(), action: action)
        let review = try await session.prepare(.sonosPlaylistTracks(selection), in: tracksZone())
        await transport.failWrite("changed")
        do { _ = try await session.execute(review); Issue.record("An unchanged queue-add receipt claimed an addition") }
        catch ClientError.uncertain { }
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes.count == 1)
    }

    @Test func skippedPlayReceiptNeverClaimsPlaybackStarted() async throws {
        let transport = TracksTransport(), session = CommandReviewSession(client: try tracksClient(transport))
        let selection = try SonosPlaylistTracksSelection(tracksDraft(), action: .play)
        let review = try await session.prepare(.sonosPlaylistTracks(selection), in: tracksZone())
        await transport.failWrite("skipped")
        let receipt = try await session.execute(review)
        #expect(receipt.partial)
        #expect(receipt.detail.hasPrefix("No playback change was reported"))
        #expect(receipt.detail.contains("The selection was not played."))
        #expect(!receipt.detail.contains("Played "))
        #expect(await transport.writes.count == 1)
    }

    @Test func lateConfirmedReceiptSurvivesCancellationAfterDispatch() async throws {
        let transport = TracksTransport(), session = CommandReviewSession(client: try tracksClient(transport))
        let selection = try SonosPlaylistTracksSelection(tracksDraft(), action: .append)
        let review = try await session.prepare(.sonosPlaylistTracks(selection), in: tracksZone())
        await transport.holdWrite()
        let task = Task { try await session.execute(review) }
        try await tracksWait { await transport.blocked }
        task.cancel(); await session.cancel(); await transport.release()
        #expect(try await task.value.title == "Selected native tracks accepted")
        #expect(await transport.writes.count == 1)
    }

    @MainActor @Test func screenSelectionSurvivesPagingRejectsStaleMenusAndClearsOnRefreshOrRoomChange() async throws {
        let transport = TracksTransport(), model = RemoteModel(client: try tracksClient(transport))
        model.resume(); defer { model.stop() }
        try await tracksModelWait { model.selectedZone != nil && !model.panelLoading }
        model.show(.sonosPlaylists)
        try await tracksModelWait { !model.panelLoading && model.sonosPlaylists.count == 1 }
        let zone = try #require(model.selectedZone)
        model.openSonosPlaylist(tracksPlaylist(), in: zone)
        try await tracksModelWait { !model.panelLoading && model.sonosPlaylistContents != nil }
        let first = try #require(model.sonosPlaylistContents), request = try #require(model.sonosPlaylistRequest)
        model.addSonosPlaylistTrack(first.items[1], from: first, request: request, in: zone)
        #expect(model.sonosPlaylistTracks.choices.map(\.item.position) == [2])
        model.sonosPlaylistPage(offset: 50)
        try await tracksModelWait { !model.panelLoading && model.sonosPlaylistContents?.offset == 50 }
        #expect(model.sonosPlaylistTracks.choices.map(\.item.position) == [2])
        model.addSonosPlaylistTrack(first.items[2], from: first, request: request, in: zone)
        #expect(model.sonosPlaylistTracks.choices.count == 1)
        let later = try #require(model.sonosPlaylistContents), next = try #require(model.sonosPlaylistRequest)
        model.addSonosPlaylistTrack(later.items[0], from: later, request: next, in: zone)
        let token = model.sonosPlaylistTracks.id, choice = try #require(model.sonosPlaylistTracks.choices.first)
        model.changeSonosPlaylistTrack(choice.id, selection: token, repeatEntry: true)
        model.changeSonosPlaylistTrack(choice.id, selection: token) // Menu token was consumed by the prior edit.
        #expect(model.sonosPlaylistTracks.choices.map(\.item.position) == [2, 51, 2])
        model.refreshSonosPlaylists()
        #expect(model.sonosPlaylistTracks.isEmpty)
        try await tracksModelWait { !model.panelLoading && model.sonosPlaylistContents?.offset == 0 }
        let fresh = try #require(model.sonosPlaylistContents), freshRequest = try #require(model.sonosPlaylistRequest)
        model.addSonosPlaylistTrack(fresh.items[0], from: fresh, request: freshRequest, in: zone)
        model.select(tracksZone("S1").id)
        #expect(model.sonosPlaylistTracks.isEmpty)
        #expect(await transport.writes.isEmpty)
    }

    @MainActor @Test func screenUsesSharedReviewAndConsumesSelectionOnceOnConfirmation() async throws {
        let transport = TracksTransport(), model = RemoteModel(client: try tracksClient(transport))
        model.resume(); defer { model.stop() }
        try await tracksModelWait { model.selectedZone != nil && !model.panelLoading }
        model.show(.sonosPlaylists)
        try await tracksModelWait { !model.panelLoading && !model.sonosPlaylists.isEmpty }
        let zone = try #require(model.selectedZone)
        model.openSonosPlaylist(tracksPlaylist(), in: zone)
        try await tracksModelWait { !model.panelLoading && model.sonosPlaylistContents != nil }
        let page = try #require(model.sonosPlaylistContents), request = try #require(model.sonosPlaylistRequest)
        model.addSonosPlaylistTrack(page.items[3], from: page, request: request, in: zone)
        let selected = model.sonosPlaylistTracks.id
        model.prepareSonosPlaylistTracks(.next, selection: selected, in: zone)
        try await tracksModelWait { model.review != nil && model.actionPhase == .idle }
        let review = try #require(model.review)
        #expect(await transport.writes.isEmpty)
        model.confirm(review.id)
        #expect(model.sonosPlaylistTracks.isEmpty)
        try await tracksModelWait { model.result?.kind == .confirmed && model.actionPhase == .idle }
        model.confirm(review.id)
        #expect(await transport.writes.count == 1)
        model.suspend()
        #expect(model.sonosPlaylistTracks.isEmpty)
    }
}
