import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

private actor CatalogClientTransport: DaemonTransport {
    private(set) var requests: [URLRequest] = []
    var status = 200
    var revision = "cache:one"
    var malformed = false

    func configure(status: Int = 200, malformed: Bool = false) {
        self.status = status; self.malformed = malformed
    }
    func send(_ request: URLRequest) async throws -> HTTPReply {
        requests.append(request)
        if status != 200 {
            return HTTPReply(data: Data(#"{"code":"NOT_READY","detail":"Library store unavailable","hint":"Check the daemon."}"#.utf8), status: status)
        }
        let fields = Dictionary(uniqueKeysWithValues: URLComponents(url: request.url!, resolvingAgainstBaseURL: false)!.queryItems!.map { ($0.name, $0.value!) })
        let category = fields["kind"]!, album = fields["album_uri"]
        let kind = category == "albums" ? "album" : category == "tracks" ? "track" : "playlist"
        var item: [String: Any] = ["source_uri": "spotify:\(kind):FixtureIdentity", "title": "Same title",
                                   "kind": kind, "explicit": false]
        if kind == "album" { item["cached_tracks"] = 2 }
        if let album { item["album_uri"] = album; item["disc_number"] = 2; item["track_number"] = 1 }
        let data = try JSONSerialization.data(withJSONObject: [
            "items": [item], "total": 1, "offset": Int(fields["offset"]!)!, "limit": 50,
            "next_offset": NSNull(), "kind": category, "album_uri": album as Any? ?? NSNull(),
            "revision": revision, "coverage": malformed ? "complete" : "cached"
        ])
        return HTTPReply(data: data, status: 200)
    }
    nonisolated func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> {
        AsyncThrowingStream { $0.finish() }
    }
}

@Suite("Native catalog daemon boundary") @MainActor struct LibraryCatalogClientTests {
    private func client(_ transport: CatalogClientTransport) throws -> DaemonClient {
        DaemonClient(endpoint: try DaemonEndpoint("https://catalog.example.invalid"), transport: transport)
    }
    private func wait(_ model: LibraryCatalogModel) async throws {
        for _ in 0..<2000 {
            if !model.loading { return }
            try await Task.sleep(for: .milliseconds(1))
        }
        throw URLError(.timedOut)
    }

    @Test func realClientEncodesExactBrowseQueryAndDoesNotReadSpeakersOrSync() async throws {
        let transport = CatalogClientTransport(), api = try client(transport)
        let request = try LibraryBrowseRequest(category: .tracks, query: "Bach + piano / études", albumUri: "spotify:album:FixtureAlbum")
        let page = try await api.libraryCatalog(request)
        #expect(page.items[0].discNumber == 2); #expect(page.coverage == "cached")
        let calls = await transport.requests
        #expect(calls.count == 1)
        let sent = try #require(calls.first)
        #expect(sent.httpMethod == "GET")
        #expect(sent.url?.path == "/library/browse")
        #expect(sent.httpBody == nil)
        #expect(sent.timeoutInterval == 15)
        #expect(sent.url?.absoluteString.contains("%2B") == true)
        let fields = Dictionary(uniqueKeysWithValues: try #require(URLComponents(url: sent.url!, resolvingAgainstBaseURL: false)?.queryItems).map { ($0.name, $0.value!) })
        #expect(fields == ["kind": "tracks", "q": "Bach + piano / études", "album_uri": "spotify:album:FixtureAlbum", "offset": "0", "limit": "50"])
    }

    @Test func everyCategoryDecodesThroughProductionClient() async throws {
        let transport = CatalogClientTransport(), api = try client(transport)
        for category in LibraryCategory.allCases {
            let page = try await api.libraryCatalog(LibraryBrowseRequest(category: category))
            #expect(page.kind == category); #expect(page.items.count == 1)
        }
        #expect(await transport.requests.count == 3)
    }

    @Test func unreadableOrUnavailableCatalogIsNotAnEmptySuccessAndNeverRetries() async throws {
        let transport = CatalogClientTransport(), api = try client(transport)
        await transport.configure(malformed: true)
        await #expect(throws: LibraryCatalogError.self) { try await api.libraryCatalog(LibraryBrowseRequest()) }
        await transport.configure(status: 503)
        await #expect(throws: APIProblem.self) { try await api.libraryCatalog(LibraryBrowseRequest()) }
        #expect(await transport.requests.count == 2)
    }

    @Test func itemActionsUseNativeCommandsAndExactURIsWithoutDispatch() async throws {
        let transport = CatalogClientTransport(), api = try client(transport)
        let model = LibraryCatalogModel(read: { try await api.libraryCatalog($0) })
        model.resume(); try await wait(model)
        let selected = try #require(model.selection(for: "spotify:album:FixtureIdentity"))
        #expect(try model.command(for: selected, action: .playNow) == .playSource(selected.item.sourceUri))
        #expect(try model.command(for: selected, action: .queueNext) == .queueAdd(selected.item.sourceUri, next: true))
        #expect(try model.command(for: selected, action: .queueLast) == .queueAdd(selected.item.sourceUri, next: false))
        #expect(await transport.requests.count == 1, "building a command is not a speaker mutation")
        model.refresh(); #expect(throws: LibraryCatalogError.self) { try model.command(for: selected, action: .playNow) }
        try await wait(model); #expect(throws: LibraryCatalogError.self) { try model.command(for: selected, action: .queueNext) }
        model.stop()
    }

    @Test func parentAlbumActionsDoNotSubstituteItsCachedTracks() async throws {
        let transport = CatalogClientTransport(), api = try client(transport)
        let model = LibraryCatalogModel(read: { try await api.libraryCatalog($0) })
        model.resume(); try await wait(model)
        model.openAlbum(try #require(model.selection(for: "spotify:album:FixtureIdentity")))
        try await wait(model)
        let album = try #require(model.albumSelection())
        #expect(try model.command(forAlbum: album, action: .queueNext) == .queueAdd("spotify:album:FixtureIdentity", next: true))
        let track = try #require(model.selection(for: "spotify:track:FixtureIdentity"))
        #expect(try model.command(for: track, action: .playNow) == .playSource("spotify:track:FixtureIdentity"))
        let calls = await transport.requests
        #expect(calls.allSatisfy { $0.httpMethod == "GET" && $0.url?.path == "/library/browse" })
        model.stop()
    }

    @Test func sameNamedSourcesRetainHouseholdQualifiedNativePayloads() throws {
        let uri = "spotify:album:FixtureIdentity"
        for household in ["S1", "S2"] {
            let zone = try DaemonClient.decode(Zone.self, from: JSONSerialization.data(withJSONObject: [
                "household": household, "coordinator_room": "Room / +", "members": ["Room / +"],
                "transport_state": "paused", "degraded": []
            ]))
            let play = try NativeCommand.playSource(uri).payload(in: zone)
            #expect(play.path == ["play"])
            #expect(play.body == ["zone": .string("Room / +@\(household)"), "source_uri": .string(uri)])
            for next in [false, true] {
                let queue = try NativeCommand.queueAdd(uri, next: next).payload(in: zone)
                #expect(queue.path == ["queue"])
                #expect(queue.body == ["zone": .string("Room / +@\(household)"), "source_uri": .string(uri), "next": .bool(next)])
            }
        }
    }
    @Test func backToAlbumsFromInvalidTrackFilterRestoresRefreshAndResume() async throws {
        let transport = CatalogClientTransport(), api = try client(transport)
        let model = LibraryCatalogModel(client: api, searchDelay: .zero)
        model.resume(); try await wait(model)
        model.openAlbum(try #require(model.selection(for: "spotify:album:FixtureIdentity")))
        try await wait(model)
        model.filter(String(repeating: "x", count: 513))
        #expect(!model.canRefresh)
        model.backToAlbums(); try await wait(model)
        #expect(model.canRefresh); #expect(model.canChoose)
        let count = await transport.requests.count
        model.suspend(); model.resume(); try await wait(model)
        #expect(model.canChoose); #expect(await transport.requests.count == count + 1)
        model.stop()
    }

}
