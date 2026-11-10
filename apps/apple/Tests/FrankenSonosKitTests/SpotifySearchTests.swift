import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

private let catalogID = "0123456789ABCDEFGHIJKL"
private func catalogZone() -> Zone {
    Zone(household: "S2", coordinatorRoom: "Studio", members: ["Studio"], transportState: "paused")
}
private func catalogPage(_ request: SpotifySearchRequest, empty: Bool = false) -> SpotifySearchPage {
    SpotifySearchPage(query: request.query, kind: request.kind, items: empty ? [] : [
        SpotifySearchItem(sourceUri: "spotify:\(request.kind.rawValue):\(catalogID)", title: "A catalog result", subtitle: "Fixture artist")
    ], more: !empty)
}
private func catalogData(kind: SpotifyCatalogKind = .track, query: String = "fixture",
                         mutate: (inout [String: Any]) -> Void = { _ in }) throws -> Data {
    var body: [String: Any] = ["query": query, "kind": kind.rawValue, "more": false,
                             "items": [["source_uri": "spotify:\(kind.rawValue):\(catalogID)", "title": "A catalog result", "subtitle": "Fixture artist"]]]
    mutate(&body)
    return try JSONSerialization.data(withJSONObject: body)
}
private func catalogDecoded(_ data: Data) throws -> SpotifySearchPage {
    try DaemonClient.decode(SpotifySearchPage.self, from: data)
}

@Suite("Native Spotify catalog search") struct SpotifySearchTests {
    @Test(arguments: SpotifyCatalogKind.allCases)
    func everyKindUsesOneCanonicalBoundedRequest(kind: SpotifyCatalogKind) throws {
        let request = try SpotifySearchRequest(query: "  Dvořák + & # ?  ", kind: kind)
        #expect(request.query == "Dvořák + & # ?" && request.limit == 10)
        #expect(request.queryItems == [URLQueryItem(name: "q", value: "Dvořák + & # ?"),
                                       URLQueryItem(name: "kind", value: kind.rawValue), URLQueryItem(name: "limit", value: "10")])
        #expect(try catalogPage(request).validated(for: request).kind == kind)
        #expect(!kind.title.isEmpty)
        #expect(kind.contains("spotify:\(kind.rawValue):\(catalogID)"))
        #expect(!kind.contains("SPOTIFY:\(kind.rawValue):\(catalogID)"))
        #expect(!kind.contains("spotify:\(kind.rawValue):short"))
        #expect(!kind.contains("spotify:\(kind.rawValue):\(catalogID)?si=tracking"))
    }

    @Test func queryLimitsCountUTF8BytesAndDoNotStripControls() throws {
        #expect(try SpotifySearchRequest(query: String(repeating: "é", count: 128)).query.utf8.count == 256)
        #expect(try SpotifySearchRequest(query: " fixture ", limit: 1).limit == 1)
        for query in ["", "   ", "fixture\n", "\tfixture", "a\u{0}b", String(repeating: "é", count: 129)] {
            #expect(throws: ContractError.self) { try SpotifySearchRequest(query: query) }
        }
        for limit in [UInt32(0), 11, .max] {
            #expect(throws: ContractError.self) { try SpotifySearchRequest(query: "fixture", limit: limit) }
        }
    }

    @Test func searchResponseRejectsWrongQueryKindDuplicatesAndUntrustedSources() throws {
        let request = try SpotifySearchRequest(query: "fixture")
        let mutations: [(inout [String: Any]) -> Void] = [
            { $0["query"] = "other" }, { $0["kind"] = "artist" }, { $0["kind"] = "future" },
            { $0.removeValue(forKey: "more") }, { $0["more"] = "false" },
            { $0["items"] = Array(repeating: ["source_uri": "spotify:track:\(catalogID)", "title": "Repeated"], count: 2) },
            { $0["items"] = [["source_uri": "https://open.spotify.com/track/\(catalogID)", "title": "Link"]] },
            { $0["items"] = [["source_uri": "spotify:episode:\(catalogID)", "title": "Wrong kind"]] },
            { $0["items"] = [["source_uri": "spotify:track:short", "title": "Bad id"]] },
            { $0["items"] = [["source_uri": "spotify:track:\(String(repeating: "é", count: 11))", "title": "Unicode id"]] },
            { $0["items"] = [["source_uri": "spotify:track:\(catalogID)", "title": "\ncontrol"]] },
            { $0["items"] = [["source_uri": "spotify:track:\(catalogID)", "title": "   "] ] },
            { $0["items"] = [["source_uri": "spotify:track:\(catalogID)", "title": String(repeating: "é", count: 2_049)]] },
            { $0["items"] = [["source_uri": "spotify:track:\(catalogID)", "title": "Title", "subtitle": "line\nbreak"]] },
            { $0["items"] = [["source_uri": "spotify:track:\(catalogID)", "title": "Title", "subtitle": String(repeating: "a", count: 4_097)]] }
        ]
        for mutation in mutations {
            let data = try catalogData(mutate: mutation)
            #expect(throws: (any Error).self) { try catalogDecoded(data).validated(for: request) }
        }
        let tooMany = try catalogData { body in
            body["items"] = [catalogID, "abcdefghijklmnopqrstuv"].map { ["source_uri": "spotify:track:\($0)", "title": "A track"] }
        }
        #expect(throws: ContractError.self) {
            try catalogDecoded(tooMany).validated(for: SpotifySearchRequest(query: "fixture", limit: 1))
        }
    }

    @Test func canonicallyEquivalentQueriesDoNotSubstituteByteDifferentRequests() throws {
        let composed = try SpotifySearchRequest(query: "é")
        let data = try catalogData(query: "e\u{301}")
        #expect(throws: ContractError.self) { try catalogDecoded(data).validated(for: composed) }
    }

    @Test func emptyResultsAndMissingSubtitleAreValidObservations() throws {
        let request = try SpotifySearchRequest(query: "fixture")
        let empty = try catalogDecoded(catalogData { $0["items"] = [] }).validated(for: request)
        #expect(empty.items.isEmpty && !empty.more)
        let page = try catalogDecoded(catalogData { body in
            body["items"] = [["source_uri": "spotify:track:\(catalogID)", "title": "No subtitle"]]
            body["more"] = true
        }).validated(for: request)
        #expect(page.items.first?.subtitle == nil && page.more)
    }

    @Test(arguments: SpotifyCatalogKind.allCases)
    func catalogKindsNormalizeForScenesAndOrdinarySourceCommands(kind: SpotifyCatalogKind) throws {
        let uri = "spotify:\(kind.rawValue):\(catalogID)"
        for pasted in [uri, "SPOTIFY:\(kind.rawValue.uppercased()):\(catalogID)",
                       "https://open.spotify.com/intl-en/embed/\(kind.rawValue)/\(catalogID)?si=tracking"] {
            #expect(try SceneDefinition.sourceURI(pasted) == uri)
            let scene = SceneDefinition(groups: [Scene.Group(coordinator: "Studio@S2", members: [],
                                                             source: Scene.Source(kind: "uri", name: nil, uri: pasted, mood: nil), playing: false)])
            let normalized = try scene.validated(zones: [catalogZone()])
            #expect(normalized.groups.first?.source.uri == uri && normalized.groups.first?.playing == false)
            for command in [NativeCommand.playSource(pasted), .queueAdd(pasted, next: true), .queueAdd(pasted, next: false)] {
                let payload = try command.payload(in: catalogZone())
                #expect(payload.body["source_uri"] == .string(pasted))
                #expect(payload.body["zone"] == .string("Studio@S2"))
            }
        }
        if kind == .artist || kind == .show {
            #expect(NativeCommand.playSource(uri).explanation.contains("up to 10"))
            #expect(NativeCommand.playSource(uri).explanation.contains("Replaces this group’s queue"))
            #expect(NativeCommand.queueAdd(uri, next: true).explanation.contains("without starting or resuming"))
        }
        for bad in ["spotify:radio:\(catalogID)", "spotify:\(kind.rawValue):short", "https://spotify.link/short"] {
            #expect(throws: ContractError.self) { try SceneDefinition.sourceURI(bad) }
        }
    }
}

private actor CatalogTransport: DaemonTransport {
    var requests: [URLRequest] = []
    var failureStatus: Int?
    func fail(_ status: Int) { failureStatus = status }
    func send(_ request: URLRequest) async throws -> HTTPReply {
        requests.append(request)
        if let failureStatus {
            let body = #"{"code":"SPOTIFY_AUTH_REQUIRED","detail":"Connect Spotify first","hint":"Open Spotify setup"}"#
            return HTTPReply(data: Data(body.utf8), status: failureStatus)
        }
        let parameters = URLComponents(url: request.url!, resolvingAgainstBaseURL: false)!.queryItems!
        let query = parameters.first { $0.name == "q" }!.value!
        let kind = SpotifyCatalogKind(rawValue: parameters.first { $0.name == "kind" }!.value!)!
        return HTTPReply(data: try catalogData(kind: kind, query: query), status: 200)
    }
    nonisolated func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> {
        AsyncThrowingStream { $0.finish() }
    }
}

@Suite("Native Spotify read-only client") struct SpotifySearchClientTests {
    @Test func usesOneBodylessEncodedGETWithoutASelectedSpeaker() async throws {
        let transport = CatalogTransport()
        let client = DaemonClient(endpoint: try DaemonEndpoint("https://music.example.test"), transport: transport)
        let request = try SpotifySearchRequest(query: "Bach + & # ?", kind: .artist)
        _ = try await client.searchSpotify(request)
        let reads = await transport.requests
        #expect(reads.count == 1)
        let read = try #require(reads.first)
        #expect(read.httpMethod == "GET" && read.httpBody == nil)
        #expect(read.timeoutInterval == 60)
        #expect(read.url?.host == "music.example.test" && read.url?.path == "/spotify/search")
        #expect(read.url?.absoluteString.contains("%2B") == true)
        #expect(URLComponents(url: read.url!, resolvingAgainstBaseURL: false)?.queryItems == request.queryItems)
    }

    @Test func ordinaryDaemonReadsRetainTheirFifteenSecondDeadline() async throws {
        let transport = CatalogTransport()
        let client = DaemonClient(endpoint: try DaemonEndpoint("https://music.example.test"), transport: transport)
        let query = try SpotifySearchRequest(query: "fixture")
        _ = try await client.get(SpotifySearchPage.self, ["spotify", "search"], query: query.queryItems)
        #expect(await transport.requests.first?.timeoutInterval == 15)
    }

    @Test(arguments: [401, 403, 429])
    func errorsPreserveRemediesAndAreNotRetried(status: Int) async throws {
        let transport = CatalogTransport()
        await transport.fail(status)
        let client = DaemonClient(endpoint: try DaemonEndpoint("https://music.example.test"), transport: transport)
        do { _ = try await client.searchSpotify(SpotifySearchRequest(query: "fixture")); Issue.record("Expected failure") }
        catch let problem as APIProblem {
            #expect(problem.code == "SPOTIFY_AUTH_REQUIRED" && problem.hint == "Open Spotify setup")
        }
        #expect(await transport.requests.count == 1)
    }
}

private actor CatalogReader {
    var requests: [SpotifySearchRequest] = []
    var active = 0, maximumActive = 0
    var hold = false, waiting = false, fail = false, empty = false
    private var gate: CheckedContinuation<Void, Never>?
    func configure(hold: Bool = false, fail: Bool = false, empty: Bool = false) {
        self.hold = hold; self.fail = fail; self.empty = empty
    }
    func release() { let old = gate; gate = nil; waiting = false; old?.resume() }
    func read(_ request: SpotifySearchRequest) async throws -> SpotifySearchPage {
        requests.append(request); active += 1; maximumActive = max(active, maximumActive)
        defer { active -= 1 }
        if hold {
            hold = false; waiting = true
            await withCheckedContinuation { gate = $0 }
        }
        if fail { throw ContractError.invalid("Spotify is cooling down; try later.") }
        return catalogPage(request, empty: empty)
    }
}
@MainActor private func catalogSettled(_ model: SpotifySearchModel) async throws {
    let deadline = ContinuousClock.now.advanced(by: .seconds(3))
    while model.loading {
        guard ContinuousClock.now < deadline else { throw URLError(.timedOut) }
        try await Task.sleep(for: .milliseconds(2))
    }
}
private func catalogWaiting(_ reader: CatalogReader) async throws {
    let deadline = ContinuousClock.now.advanced(by: .seconds(3))
    while !(await reader.waiting) {
        guard ContinuousClock.now < deadline else { throw URLError(.timedOut) }
        try await Task.sleep(for: .milliseconds(2))
    }
}

@Suite("Native Spotify search lifecycle") @MainActor struct SpotifySearchModelTests {
    @Test func visibilityTypingAndKindChangesNeverRequestUntilSearch() async throws {
        let reader = CatalogReader(), time = Date(timeIntervalSince1970: 123)
        let model = SpotifySearchModel(read: { try await reader.read($0) }, now: { time })
        defer { model.stop() }
        #expect(!model.canSearch && !model.canChoose)
        model.resume(); model.edit("fixture"); model.chooseKind(.show)
        #expect(model.canSearch && !model.canChoose)
        #expect(await reader.requests.isEmpty)
        model.search(); try await catalogSettled(model)
        let first = try #require(model.observation)
        #expect(model.canChoose && first.receivedAt == time && first.page.kind == .show)
        #expect(await reader.requests.count == 1)
        model.edit("different")
        #expect(!model.canChoose)
        model.edit("fixture")
        #expect(!model.canChoose)
        #expect(throws: ContractError.self) { try model.selection(first.page.items[0], from: first.id, in: catalogZone(), action: .playNow) }
        model.search(); try await catalogSettled(model)
        #expect(model.canChoose && model.observation?.id != first.id)
        model.chooseKind(.artist)
        #expect(!model.canChoose)
        #expect(await reader.requests.count == 2)
    }

    @Test func canonicallyEquivalentQueryEditRetiresSelection() async throws {
        let reader = CatalogReader(), model = SpotifySearchModel(read: { try await reader.read($0) })
        defer { model.stop() }
        model.resume(); model.edit("é"); model.search(); try await catalogSettled(model)
        #expect(model.canChoose)
        model.edit("e\u{301}")
        #expect(!model.canChoose)
    }

    @Test func obsoleteSearchDrainsBeforeAnotherQueryAndNeverPublishes() async throws {
        let reader = CatalogReader(), model = SpotifySearchModel(read: { try await reader.read($0) })
        defer { model.stop() }
        await reader.configure(hold: true)
        model.resume(); model.edit("first"); model.search()
        try await catalogWaiting(reader)
        model.edit("second"); model.chooseKind(.episode); model.search()
        #expect(model.loading && !model.canSearch && !model.canChoose)
        #expect(await reader.requests.count == 1)
        await reader.release(); try await catalogSettled(model)
        #expect(model.observation == nil && model.problem == nil && model.canSearch)
        #expect(await reader.maximumActive == 1)
        #expect(await reader.requests.count == 1)
        model.search(); try await catalogSettled(model)
        #expect(model.observation?.page.query == "second" && model.observation?.page.kind == .episode)
        #expect(await reader.maximumActive == 1)
        #expect(await reader.requests.count == 2)
    }

    @Test func cancellationSuspensionAndStopNeverReviveOrRetryResults() async throws {
        for operation in ["cancel", "suspend", "stop"] {
            let reader = CatalogReader(), model = SpotifySearchModel(read: { try await reader.read($0) })
            await reader.configure(hold: true)
            model.resume(); model.edit("fixture"); model.search(); try await catalogWaiting(reader)
            switch operation { case "cancel": model.cancel(); case "suspend": model.suspend(); default: model.stop() }
            model.resume(); model.search()
            #expect(model.loading && !model.canChoose)
            await reader.release(); try await catalogSettled(model)
            #expect(model.observation == nil && !model.canChoose)
            #expect(await reader.requests.count == 1)
            if operation == "stop" { #expect(!model.canSearch && !model.active) }
            model.stop()
        }
    }

    @Test func cancelledBeforeTaskStartsDoesNotRequest() async throws {
        let reader = CatalogReader(), model = SpotifySearchModel(read: { try await reader.read($0) })
        defer { model.stop() }
        model.resume(); model.edit("fixture"); model.search(); model.suspend()
        try await catalogSettled(model)
        #expect(await reader.requests.isEmpty)
    }

    @Test func returnedEmptyPageAndFailedRefreshRetainDifferentStates() async throws {
        let reader = CatalogReader(), model = SpotifySearchModel(read: { try await reader.read($0) })
        defer { model.stop() }
        await reader.configure(empty: true)
        model.resume(); model.edit("fixture"); model.search(); try await catalogSettled(model)
        let empty = try #require(model.observation)
        #expect(empty.page.items.isEmpty && model.isCurrent && model.problem == nil)
        await reader.configure(fail: true)
        model.search(); try await catalogSettled(model)
        #expect(model.observation?.id == empty.id && !model.isCurrent && !model.canChoose)
        #expect(model.problem?.contains("cooling down") == true)
        #expect(await reader.requests.count == 2)
        model.suspend(); model.resume()
        #expect(!model.canChoose)
        #expect(await reader.requests.count == 2)
    }

    @Test func onlyExactCurrentResultsCanEnterExistingSourceActions() async throws {
        let reader = CatalogReader(), model = SpotifySearchModel(read: { try await reader.read($0) })
        defer { model.stop() }
        model.resume(); model.edit("fixture"); model.chooseKind(.artist); model.search(); try await catalogSettled(model)
        let observed = try #require(model.observation), item = observed.page.items[0]
        let expected: [NativeCommand] = [.playSource(item.sourceUri), .queueAdd(item.sourceUri, next: true), .queueAdd(item.sourceUri, next: false)]
        for (action, command) in zip(SpotifySearchSelection.Action.allCases, expected) {
            let selected = try model.selection(item, from: observed.id, in: catalogZone(), action: action)
            #expect(selected.command == command && selected.zone == catalogZone())
        }
        let forged = SpotifySearchItem(sourceUri: "spotify:artist:abcdefghijklmnopqrstuv", title: item.title, subtitle: nil)
        #expect(throws: ContractError.self) { try model.selection(forged, from: observed.id, in: catalogZone(), action: .playNow) }
        #expect(throws: ContractError.self) { try model.selection(item, from: UUID(), in: catalogZone(), action: .playNow) }
        model.suspend(); model.resume()
        #expect(throws: ContractError.self) { try model.selection(item, from: observed.id, in: catalogZone(), action: .queueLast) }
        #expect(await reader.requests.count == 1)
    }
}
