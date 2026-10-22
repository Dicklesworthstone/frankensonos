import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

private actor PlaylistBrowserTransport: DaemonTransport {
    var requests: [URLRequest] = []
    var listingRevision = "listing:1"
    private var block = false
    private var gate: CheckedContinuation<Void, Never>?
    var held = false
    func changeListing() { listingRevision = "listing:2" }
    func blockDetail() { block = true }
    func release() { let old = gate; gate = nil; held = false; old?.resume() }
    func send(_ request: URLRequest) async throws -> HTTPReply {
        requests.append(request)
        #expect(request.httpMethod == nil || request.httpMethod == "GET")
        #expect(request.httpBody == nil)
        let parts = URLComponents(url: request.url!, resolvingAgainstBaseURL: false)!
        func q(_ name: String) -> String? { parts.queryItems?.first { $0.name == name }?.value }
        if parts.path == "/library/playlist" {
            if block { block = false; held = true; await withCheckedContinuation { gate = $0 } }
            return try json(["playlist_uri": q("playlist_uri")!, "name": "Road trip", "q": q("q") ?? "",
                             "coverage": "partial", "fetched_at": 1800000000, "source_total": 20,
                             "examined": 4, "cached_total": 2, "total": 2, "offset": 0, "limit": 50,
                             "revision": "contents:1", "items": [1, 3].map {
                                ["position": $0, "source_uri": "spotify:track:Repeated", "title": "Literal track",
                                 "subtitle": "Fixture artist", "explicit": false] as [String: Any]
                             }])
        }
        #expect(parts.path == "/library/browse")
        let category = q("kind") ?? "albums", offset = Int(q("offset") ?? "0")!
        let kind = category == "tracks" ? "track" : category == "albums" ? "album" : "playlist"
        let items: [[String: Any]] = (offset..<min(offset + 50, 60)).map { index in
            var item: [String: Any] = ["source_uri": "spotify:\(kind):P\(index)", "title": "Road trip \(index)",
                                       "kind": kind, "explicit": false]
            if kind == "album" { item["cached_tracks"] = 1 }
            if let album = q("album_uri") { item["album_uri"] = album }
            return item
        }
        var body: [String: Any] = ["items": items, "total": 60, "offset": offset, "limit": 50,
                                   "kind": category, "coverage": "cached", "revision": listingRevision]
        if offset == 0 { body["next_offset"] = 50 }
        if let album = q("album_uri") { body["album_uri"] = album }
        return try json(body)
    }
    private func json(_ body: [String: Any]) throws -> HTTPReply {
        HTTPReply(data: try JSONSerialization.data(withJSONObject: body), status: 200)
    }
    nonisolated func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> { AsyncThrowingStream { $0.finish() } }
}
@MainActor private func browserWait(_ flag: () -> Bool) async throws {
    let deadline = ContinuousClock.now.advanced(by: .seconds(3))
    while !flag() {
        guard ContinuousClock.now < deadline else { throw URLError(.timedOut) }
        try await Task.sleep(for: .milliseconds(2))
    }
}
@MainActor private func playlistBrowser(_ transport: PlaylistBrowserTransport) throws -> LibraryCatalogModel {
    LibraryCatalogModel(client: DaemonClient(endpoint: try DaemonEndpoint("https://music.example.test"), transport: transport), searchDelay: .zero)
}
@MainActor private func openBrowserPlaylist(_ model: LibraryCatalogModel) async throws {
    model.resume(); model.browse(.playlists)
    try await browserWait { model.canChoose }
    try model.openPlaylist(#require(model.selection(for: "spotify:playlist:P0")))
    try await browserWait { model.playlist.canChoose }
}

@Suite("Native playlist screen navigation") struct PlaylistBrowserTests {
    @Test @MainActor func drilldownClearsOnlyTrackFilterAndRestoresListingPageAndFilter() async throws {
        let transport = PlaylistBrowserTransport(), browser = try playlistBrowser(transport)
        browser.resume(); browser.browse(.playlists)
        try await browserWait { browser.canChoose }
        browser.filter("Road trip + &"); try await browserWait { browser.canChoose }
        browser.nextPage(); try await browserWait { browser.canChoose }
        let selection = try #require(browser.selection(for: "spotify:playlist:P50"))
        try browser.openPlaylist(selection); try await browserWait { browser.playlist.canChoose }
        #expect(browser.playlist.query.isEmpty && browser.request.query == "Road trip + &")
        #expect(browser.request.offset == 50 && !browser.canChoose)
        #expect(browser.resolve(selection) == nil)
        browser.filter("hidden field cannot alter listing")
        #expect(browser.request.query == "Road trip + &")
        browser.playlist.edit("Literal"); browser.refresh()
        try await browserWait { browser.playlist.canChoose }
        #expect(browser.playlist.observation?.request.query == "Literal")
        browser.backToPlaylists(); try await browserWait { browser.canChoose }
        #expect(!browser.playlist.isOpen && browser.request.offset == 50)
        #expect(browser.request.query == "Road trip + &")
        #expect(browser.resolve(selection) == nil)
        #expect(await transport.requests.allSatisfy { $0.url?.path.hasPrefix("/library/") == true })
        browser.stop()
    }

    @Test @MainActor func changedListingOnReturnRequiresExplicitRestart() async throws {
        let transport = PlaylistBrowserTransport(), browser = try playlistBrowser(transport)
        try await openBrowserPlaylist(browser)
        await transport.changeListing()
        browser.backToPlaylists()
        try await browserWait { !browser.loading }
        #expect(browser.restartRequired && !browser.canChoose)
        browser.refresh(); try await browserWait { browser.canChoose }
        #expect(browser.page?.revision == "listing:2")
        browser.stop()
    }

    @Test @MainActor func staleMenuAndNonPlaylistCannotOpenDetails() async throws {
        let transport = PlaylistBrowserTransport(), browser = try playlistBrowser(transport)
        browser.resume(); try await browserWait { browser.canChoose }
        let album = try #require(browser.selection(for: "spotify:album:P0"))
        #expect(throws: LibraryCatalogError.self) { try browser.openPlaylist(album) }
        browser.browse(.playlists); try await browserWait { browser.canChoose }
        let selection = try #require(browser.selection(for: "spotify:playlist:P0"))
        browser.refresh(); try await browserWait { browser.canChoose }
        #expect(throws: LibraryCatalogError.self) { try browser.openPlaylist(selection) }
        #expect(!browser.playlist.isOpen)
        #expect(await transport.requests.allSatisfy { $0.url?.path == "/library/browse" })
        browser.stop()
    }

    @Test @MainActor func parentSuspensionStopsDetailAndResumeDoesNotSilentlyReadAgain() async throws {
        let transport = PlaylistBrowserTransport(), browser = try playlistBrowser(transport)
        try await openBrowserPlaylist(browser)
        let observation = try #require(browser.playlist.observation)
        let before = await transport.requests.count
        browser.suspend(); browser.resume()
        #expect(browser.playlist.isOpen && browser.playlist.active && !browser.playlist.canChoose)
        #expect(browser.playlist.observation?.id == observation.id)
        #expect(await transport.requests.count == before)
        browser.refresh(); try await browserWait { browser.playlist.canChoose }
        #expect(browser.playlist.observation?.id != observation.id)
        browser.stop(); browser.resume()
        #expect(browser.playlist.observation == nil && !browser.playlist.isOpen)
    }

    @Test @MainActor func closedDetailCannotRepopulateAnotherCategoryFromLateRead() async throws {
        let transport = PlaylistBrowserTransport(), browser = try playlistBrowser(transport)
        browser.resume(); browser.browse(.playlists); try await browserWait { browser.canChoose }
        await transport.blockDetail()
        try browser.openPlaylist(#require(browser.selection(for: "spotify:playlist:P0")))
        let deadline = ContinuousClock.now.advanced(by: .seconds(3))
        while !(await transport.held) {
            guard ContinuousClock.now < deadline else { throw URLError(.timedOut) }
            try await Task.sleep(for: .milliseconds(2))
        }
        browser.browse(.albums)
        await transport.release()
        try await browserWait { browser.canChoose && !browser.playlist.loading }
        #expect(browser.request.category == .albums && browser.playlist.observation == nil)
        #expect(!browser.playlist.isOpen)
        browser.stop()
    }
}
