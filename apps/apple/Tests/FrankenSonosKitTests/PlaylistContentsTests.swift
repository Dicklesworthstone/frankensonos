import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

private let playlistURI = "spotify:playlist:FixtureA"
private func playlistZone(_ household: String = "S2") -> Zone {
    Zone(household: household, coordinatorRoom: "Studio", members: ["Studio"], transportState: "playing")
}
private func playlistData(offset: UInt32 = 0, query: String = "", revision: String = "cached:r1",
                          total: UInt32 = 75, coverage: String = "complete",
                          mutate: (inout [String: Any]) -> Void = { _ in }) throws -> Data {
    let count = min(50, total > offset ? total - offset : 0)
    let items: [[String: Any]] = (0..<count).map { index in
        let occurrence = offset + index
        return ["position": occurrence * 2 + 1,
                "source_uri": "spotify:track:" + (occurrence == 0 || occurrence == 25 ? "Repeated" : "T\(occurrence)"),
                "title": "Track \(occurrence)", "subtitle": "Fixture artist", "album": "Fixture album",
                "album_uri": "spotify:album:FixtureAlbum", "duration_secs": 120, "explicit": occurrence == 1]
    }
    let end = offset + count
    var body: [String: Any] = ["playlist_uri": playlistURI, "name": "Fixture playlist", "q": query,
                              "coverage": coverage, "fetched_at": Int64(1_800_000_000),
                              "source_total": total * 2, "examined": total * 2, "cached_total": total,
                              "total": total, "offset": offset, "limit": 50, "revision": revision, "items": items]
    if end < total { body["next_offset"] = end }
    mutate(&body)
    return try JSONSerialization.data(withJSONObject: body)
}
private func playlistPage(offset: UInt32 = 0, query: String = "", revision: String = "cached:r1", total: UInt32 = 75) throws -> PlaylistContentsPage {
    try DaemonClient.decode(PlaylistContentsPage.self, from: playlistData(offset: offset, query: query, revision: revision, total: total))
}
private func playlistRequest(offset: UInt32 = 0, revision: String? = nil, query: String = "") throws -> PlaylistContentsRequest {
    try PlaylistContentsRequest(playlistUri: playlistURI, query: query, offset: offset, expectedRevision: revision)
}
private func playlistSelection(_ action: PlaylistTrackSelection.Action = .queueLast, home: String = "S2") throws -> PlaylistTrackSelection {
    let page = try playlistPage()
    return try PlaylistTrackSelection(item: page.items[25], page: page, request: playlistRequest(), in: playlistZone(home), action: action)
}

private actor PlaylistTransport: DaemonTransport {
    var requests: [URLRequest] = []
    var revision = "cached:r1", track = "fixture:playing1"
    var failRead = false, changedMetadata = false
    var writeFault: String?
    var holdPath: String?
    var held = false
    var maximumActive = 0, active = 0
    private var gate: CheckedContinuation<Void, Never>?
    func configure(revision: String? = nil, track: String? = nil, failRead: Bool = false,
                   changedMetadata: Bool = false, writeFault: String? = nil, hold: String? = nil) {
        if let revision { self.revision = revision }
        if let track { self.track = track }
        self.failRead = failRead; self.changedMetadata = changedMetadata; self.writeFault = writeFault; holdPath = hold
    }
    func release() { let old = gate; gate = nil; held = false; old?.resume() }
    func writes() -> [URLRequest] { requests.filter { $0.httpMethod == "POST" } }
    func send(_ request: URLRequest) async throws -> HTTPReply {
        requests.append(request); active += 1; maximumActive = max(active, maximumActive)
        defer { active -= 1 }
        let path = request.url!.path.removingPercentEncoding ?? request.url!.path
        if path == holdPath {
            holdPath = nil; held = true
            await withCheckedContinuation { gate = $0 }
        }
        if request.httpMethod == "POST" {
            if writeFault == "lost" { throw URLError(.networkConnectionLost) }
            if writeFault == "malformed" { return HTTPReply(data: Data("{}".utf8), status: 200) }
            if writeFault == "redirect" { return HTTPReply(data: Data(), status: 307) }
            #expect(path == "/play" || path == "/queue")
            return HTTPReply(data: Data((path == "/play"
                ? #"{"done":"Playing the track","changed":true}"#
                : #"{"done":"Queued the track","first":9,"count":1,"playing_queue":true}"#).utf8), status: 200)
        }
        #expect(request.httpBody == nil)
        if path == "/zones" {
            let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
            return HTTPReply(data: try encoder.encode([playlistZone("S1"), playlistZone("S2")]), status: 200)
        }
        if path.hasSuffix("/state") {
            let house = path.contains("@S1/") ? "S1" : "S2"
            return HTTPReply(data: Data("""
                {"zone":{"household":"\(house)","coordinator_room":"Studio","members":["Studio"],"transport_state":"playing"},
                "transport_state":"playing","volume":20,"mute":false,"track":{"uri":"\(track)","duration_secs":120,"position_secs":1,"queue_position":1}}
                """.utf8), status: 200)
        }
        #expect(path == "/library/playlist")
        let query = URLComponents(url: request.url!, resolvingAgainstBaseURL: false)!.queryItems!
        func value(_ key: String) -> String? { query.first { $0.name == key }?.value }
        if failRead { return HTTPReply(data: Data(#"{"code":"POLICY_DENIED","detail":"Not allowed","hint":"Check policy"}"#.utf8), status: 403) }
        if let expected = value("expected_revision"), expected != revision {
            return HTTPReply(data: Data(#"{"code":"INVALID_ARGUMENT","detail":"The cached playlist changed","hint":"Restart"}"#.utf8), status: 400)
        }
        let data = try playlistData(offset: UInt32(value("offset")!)!, query: value("q") ?? "", revision: revision) { body in
            if self.changedMetadata {
                var items = body["items"] as! [[String: Any]]
                for index in items.indices { items[index]["subtitle"] = "Replaced artist" }
                body["items"] = items
            }
        }
        return HTTPReply(data: data, status: 200)
    }
    nonisolated func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> { AsyncThrowingStream { $0.finish() } }
}
private func playlistClient(_ transport: PlaylistTransport) throws -> DaemonClient {
    DaemonClient(endpoint: try DaemonEndpoint("https://music.example.test"), transport: transport)
}
private func playlistHeld(_ transport: PlaylistTransport) async throws {
    let deadline = ContinuousClock.now.advanced(by: .seconds(3))
    while !(await transport.held) {
        guard ContinuousClock.now < deadline else { throw URLError(.timedOut) }
        try await Task.sleep(for: .milliseconds(2))
    }
}
@MainActor private func playlistSettled(_ model: PlaylistContentsModel) async throws {
    let deadline = ContinuousClock.now.advanced(by: .seconds(3))
    while model.loading {
        guard ContinuousClock.now < deadline else { throw URLError(.timedOut) }
        try await Task.sleep(for: .milliseconds(2))
    }
}

@Suite("Native cached playlist occurrences") struct PlaylistContentsTests {
    @Test func requestUsesCanonicalIdentityAndScalarBoundedLiteralFilter() throws {
        let request = try playlistRequest(offset: .max, revision: "opaque:+/=?", query: "  é + & ? #  ")
        #expect(request.query == "é + & ? #")
        #expect(request.offset == UInt32.max)
        #expect(try PlaylistContentsRequest(playlistUri: playlistURI, query: String(repeating: "é", count: 512)).query.utf8.count == 1024)
        for bad in ["", "spotify:album:A", "spotify:playlist:bad/name", "spotify:playlist:" + String(repeating: "a", count: 129)] {
            #expect(throws: ContractError.self) { try PlaylistContentsRequest(playlistUri: bad) }
        }
        #expect(throws: ContractError.self) { try playlistRequest(offset: 50) }
        for bad in ["", " ", "line\nbreak", String(repeating: "r", count: 257)] {
            #expect(throws: ContractError.self) { try playlistRequest(revision: bad) }
        }
        #expect(throws: ContractError.self) { try playlistRequest(query: String(repeating: "é", count: 513)) }
        #expect(throws: ContractError.self) { try playlistRequest(query: "bad\u{0}filter") }
    }

    @Test func sparsePositionsAndRepeatedTracksRemainDifferentOccurrences() throws {
        let page = try playlistPage().validated(for: playlistRequest())
        #expect(page.items.count == 50 && page.nextOffset == 50)
        #expect(page.items[0].sourceUri == page.items[25].sourceUri)
        #expect(page.items[0].id == 1 && page.items[25].id == 51)
        #expect(page.items[1].explicit)
        let last = try playlistPage(offset: 50).validated(for: playlistRequest(offset: 50, revision: "cached:r1"))
        #expect(last.items.count == 25 && last.items.first?.position == 101 && last.nextOffset == nil)
    }

    @Test func missingEmptyUnsupportedAndPartialObservationsHaveDifferentMeanings() throws {
        func page(_ coverage: String, examined: Int, source: Int?, fetched: Int64?, revision: String?) throws -> PlaylistContentsPage {
            let data = try playlistData(total: 0, coverage: coverage) { b in
                b["examined"] = examined; b["source_total"] = source; b["fetched_at"] = fetched; b["revision"] = revision
            }
            return try DaemonClient.decode(PlaylistContentsPage.self, from: data).validated(for: playlistRequest())
        }
        let absent = try page("not_cached", examined: 0, source: nil, fetched: nil, revision: nil)
        let empty = try page("complete", examined: 0, source: 0, fetched: 0, revision: "r")
        let unsupported = try page("complete", examined: 6, source: 6, fetched: 1, revision: "r")
        let partial = try page("partial", examined: 2, source: 6, fetched: 1, revision: "r")
        #expect(absent.coverageText.contains("not been cached"))
        #expect(empty.emptyText.contains("empty playlist"))
        #expect(unsupported.emptyText.contains("no supported playable"))
        #expect(partial.emptyText.contains("partial"))
        let unknownTotal = try page("partial", examined: 4, source: nil, fetched: Int64.max, revision: "r")
        #expect(unknownTotal.fetchedAt == Int64.max)
    }

    @Test func malformedCoverageCountsIdentitiesAndMetadataAreRejectedWhole() throws {
        let mutations: [(inout [String: Any]) -> Void] = [
            { $0["playlist_uri"] = "spotify:playlist:Other" }, { $0["q"] = "changed" },
            { $0["offset"] = 1 }, { $0["limit"] = 500 }, { $0["next_offset"] = 49 },
            { $0["cached_total"] = 74 }, { $0["examined"] = 74 }, { $0["source_total"] = 151 },
            { $0["coverage"] = "not_cached" }, { $0["coverage"] = "future" },
            { $0.removeValue(forKey: "fetched_at") }, { $0["revision"] = "" },
            { $0["items"] = [] }, { $0["total"] = -1 }, { $0["total"] = 1.5 },
            { b in var rows = b["items"] as! [[String: Any]]; rows[1]["position"] = 1; b["items"] = rows },
            { b in var rows = b["items"] as! [[String: Any]]; rows[0]["source_uri"] = "spotify:episode:X"; b["items"] = rows },
            { b in var rows = b["items"] as! [[String: Any]]; rows[0]["title"] = String(repeating: "a", count: 16_385); b["items"] = rows },
            { b in var rows = b["items"] as! [[String: Any]]; rows[0]["explicit"] = "false"; b["items"] = rows },
        ]
        for mutate in mutations {
            let data = try playlistData(mutate: mutate)
            #expect(throws: (any Error).self) {
                try DaemonClient.decode(PlaylistContentsPage.self, from: data).validated(for: playlistRequest())
            }
        }
        #expect(throws: ContractError.self) { try playlistPage().validated(for: playlistRequest(revision: "old")) }
    }

    @Test func ordinaryReaderUsesOnlyOneBodylessEncodedCacheGET() async throws {
        let transport = PlaylistTransport(), client = try playlistClient(transport)
        _ = try await client.playlistContents(playlistRequest(query: "Bach + & #"))
        let requests = await transport.requests
        #expect(requests.count == 1)
        let request = try #require(requests.first)
        #expect(request.httpBody == nil && request.url?.path == "/library/playlist")
        #expect(request.url?.absoluteString.contains("%2B") == true)
        let params = URLComponents(url: request.url!, resolvingAgainstBaseURL: false)!.queryItems!
        #expect(params.contains(URLQueryItem(name: "q", value: "Bach + & #")))
        #expect(params.contains(URLQueryItem(name: "playlist_uri", value: playlistURI)))
        #expect(!params.contains { $0.name == "zone" })
    }

    @Test func cacheReadErrorsAreNotUncertainWritesOrAutomaticallyRetried() async throws {
        let transport = PlaylistTransport(), client = try playlistClient(transport)
        await transport.configure(failRead: true)
        do { _ = try await client.playlistContents(playlistRequest()); Issue.record("Expected denial") }
        catch let error as APIProblem { #expect(error.code == "POLICY_DENIED" && error.hint == "Check policy") }
        #expect(await transport.requests.count == 1)
        #expect(await transport.writes().isEmpty)
    }

    @Test @MainActor func browserReadsOnlyExplicitlyAndRetiresEditedAndHiddenSelections() async throws {
        let transport = PlaylistTransport(), model = try PlaylistContentsModel(client: playlistClient(transport))
        #expect(await transport.requests.isEmpty)
        try model.open(playlistUri: playlistURI, name: "Fixture")
        try await playlistSettled(model)
        let first = try #require(model.observation)
        _ = try model.selection(first.page.items[0], from: first.id, in: playlistZone(), action: .playNow)
        model.edit("different")
        #expect(!model.isCurrent)
        #expect(await transport.requests.count == 1)
        #expect(throws: ContractError.self) { try model.selection(first.page.items[0], from: first.id, in: playlistZone(), action: .playNow) }
        model.edit("")
        #expect(!model.canChoose)
        model.refresh(); try await playlistSettled(model)
        #expect(model.isCurrent && model.observation?.id != first.id)
        model.suspend(); model.resume()
        #expect(!model.canChoose && model.canRefresh)
        #expect(await transport.requests.count == 2)
        model.stop(); model.resume()
        #expect(!model.canRefresh && model.observation == nil)
        #expect(throws: ContractError.self) { try model.open(playlistUri: playlistURI, name: "Fixture") }
    }

    @Test @MainActor func pagesReplaceNotAccumulateAndCarryRevisionBothDirections() async throws {
        let transport = PlaylistTransport(), model = try PlaylistContentsModel(client: playlistClient(transport))
        try model.open(playlistUri: playlistURI, name: "Fixture"); try await playlistSettled(model)
        model.nextPage(); try await playlistSettled(model)
        #expect(model.observation?.page.items.count == 25 && model.observation?.request.offset == 50)
        #expect(model.observation?.request.expectedRevision == "cached:r1")
        model.previousPage(); try await playlistSettled(model)
        #expect(model.observation?.page.items.count == 50 && model.observation?.request.offset == 0)
        #expect(model.observation?.request.expectedRevision == "cached:r1")
        await transport.configure(revision: "cached:r2")
        model.nextPage(); try await playlistSettled(model)
        #expect(!model.isCurrent && model.problem != nil && model.observation?.request.offset == 0)
        #expect(!model.canGoForward)
        model.refresh(); try await playlistSettled(model)
        #expect(model.isCurrent && model.observation?.page.revision == "cached:r2")
        model.close()
    }

    @Test @MainActor func cancellationDrainsAndLateResultsCannotReopenHiddenPlaylist() async throws {
        let transport = PlaylistTransport(), model = try PlaylistContentsModel(client: playlistClient(transport))
        await transport.configure(hold: "/library/playlist")
        try model.open(playlistUri: playlistURI, name: "Fixture")
        try await playlistHeld(transport)
        model.cancel(); model.refresh()
        #expect(model.loading && !model.canRefresh)
        model.close(); await transport.release(); try await playlistSettled(model)
        #expect(model.observation == nil && !model.isOpen)
        #expect(await transport.requests.count == 1)
        #expect(await transport.maximumActive == 1)
    }

    @Test @MainActor func immediateCancelNeverInvokesReaderAndInvalidFiltersNeverUseOldQuery() async throws {
        let transport = PlaylistTransport(), model = try PlaylistContentsModel(client: playlistClient(transport))
        try model.open(playlistUri: playlistURI, name: "Fixture"); model.cancel()
        try await playlistSettled(model)
        #expect(await transport.requests.isEmpty)
        model.edit(String(repeating: "x", count: 513)); model.refresh()
        #expect(!model.canRefresh)
        #expect(await transport.requests.isEmpty)
        model.edit(""); model.refresh(); try await playlistSettled(model)
        #expect(model.isCurrent)
        #expect(await transport.requests.count == 1)
        model.close()
    }
}

@Suite("Playlist selections through the shared writer") struct PlaylistContentsCommandTests {
    @Test(arguments: ["S1", "S2"], PlaylistTrackSelection.Action.allCases)
    func reviewedOccurrenceSendsOneExactTrackAction(home: String, action: PlaylistTrackSelection.Action) async throws {
        let transport = PlaylistTransport(), session = try CommandReviewSession(client: playlistClient(transport))
        let chosen = try playlistSelection(action, home: home)
        let review = try await session.prepare(.playlistTrack(chosen), in: playlistZone(home))
        #expect(await transport.writes().isEmpty)
        _ = try await session.execute(review)
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        let writes = await transport.writes()
        #expect(writes.count == 1)
        let request = try #require(writes.first)
        let body = try JSONDecoder().decode([String: JSONValue].self, from: #require(request.httpBody))
        var expected: [String: JSONValue] = ["zone": .string("Studio@\(home)"), "source_uri": .string("spotify:track:Repeated")]
        if action != .playNow { expected["next"] = .bool(action == .queueNext) }
        #expect(body == expected)
        #expect(request.url?.path == (action == .playNow ? "/play" : "/queue"))
        #expect(chosen.explanation.contains("Original playlist position: 51"))
        #expect(await transport.requests.filter { $0.url?.path == "/library/playlist" }.count == 2)
        #expect(await transport.requests.allSatisfy { !($0.url?.path.hasSuffix("/queue/play") ?? false) })
    }

    @Test(arguments: [false, true])
    func changedCacheOrOccurrenceStopsBeforeAnyWrite(afterReview: Bool) async throws {
        for metadata in [false, true] {
            let transport = PlaylistTransport(), session = try CommandReviewSession(client: playlistClient(transport))
            let command = NativeCommand.playlistTrack(try playlistSelection())
            let review = afterReview ? try await session.prepare(command, in: playlistZone()) : nil
            await transport.configure(revision: metadata ? nil : "new", changedMetadata: metadata)
            if let review { await #expect(throws: (any Error).self) { try await session.execute(review) } }
            else { await #expect(throws: (any Error).self) { try await session.prepare(command, in: playlistZone()) } }
            #expect(await transport.writes().isEmpty)
        }
    }

    @Test func selectionCannotRetargetAnIdenticallyNamedOtherHousehold() async throws {
        let transport = PlaylistTransport(), session = try CommandReviewSession(client: playlistClient(transport))
        let command = NativeCommand.playlistTrack(try playlistSelection(home: "S1"))
        await #expect(throws: ContractError.self) { try await session.prepare(command, in: playlistZone("S2")) }
        #expect(await transport.requests.isEmpty)
    }

    @Test func appendAllowsNaturalAdvanceButPlayAndQueueNextRequireReviewAgain() async throws {
        for action in PlaylistTrackSelection.Action.allCases {
            let transport = PlaylistTransport(), session = try CommandReviewSession(client: playlistClient(transport))
            let review = try await session.prepare(.playlistTrack(playlistSelection(action)), in: playlistZone())
            await transport.configure(track: "fixture:nextTrack")
            if action == .queueLast { _ = try await session.execute(review); #expect(await transport.writes().count == 1) }
            else { await #expect(throws: ContractError.self) { try await session.execute(review) }; #expect(await transport.writes().isEmpty) }
        }
    }

    @Test func cancelledFinalCacheReadCannotSendAndRetainsSharedGateUntilDrain() async throws {
        let transport = PlaylistTransport(), session = try CommandReviewSession(client: playlistClient(transport))
        let review = try await session.prepare(.playlistTrack(playlistSelection()), in: playlistZone())
        await transport.configure(hold: "/library/playlist")
        let task = Task { try await session.execute(review) }
        try await playlistHeld(transport); await session.cancel()
        await #expect(throws: ContractError.self) { try await session.prepare(.pause, in: playlistZone()) }
        await transport.release()
        await #expect(throws: CancellationError.self) { try await task.value }
        #expect(await transport.writes().isEmpty)
    }

    @Test func lostMalformedAndRedirectedWritesStayUncertainWithoutReplay() async throws {
        for fault in ["lost", "malformed", "redirect"] {
            let transport = PlaylistTransport(), session = try CommandReviewSession(client: playlistClient(transport))
            let review = try await session.prepare(.playlistTrack(playlistSelection()), in: playlistZone())
            await transport.configure(writeFault: fault)
            do { _ = try await session.execute(review); Issue.record("Expected uncertain result") }
            catch let error as ClientError { guard case .uncertain = error else { Issue.record("Not uncertain"); return } }
            await #expect(throws: ContractError.self) { try await session.execute(review) }
            #expect(await transport.writes().count == 1)
        }
    }
}
