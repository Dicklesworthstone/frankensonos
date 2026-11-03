import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

private func catalogPage(_ request: LibraryBrowseRequest, total: UInt32 = 71,
                         revision: String = "catalog:one", mutate: (inout [String: Any]) -> Void = { _ in }) throws -> LibraryCatalogPage {
    try DaemonClient.decode(LibraryCatalogPage.self, from: Data(catalogJSON(request, total: total, revision: revision, mutate: mutate).utf8))
}

private func catalogJSON(_ request: LibraryBrowseRequest, total: UInt32 = 71,
                         revision: String = "catalog:one", mutate: (inout [String: Any]) -> Void = { _ in }) throws -> String {
    let count = min(50, total > request.offset ? total - request.offset : 0)
    let kind = request.category == .tracks ? "track" : request.category == .albums ? "album" : "playlist"
    let items: [[String: Any]] = (0..<count).map { n in
        var item: [String: Any] = ["source_uri": "spotify:\(kind):fixture\(request.offset + n)",
            "title": "Same title", "kind": kind, "subtitle": "Fixture artist", "explicit": n == 1]
        if request.category == .albums { item["cached_tracks"] = 4 }
        if let album = request.albumUri {
            item["album_uri"] = album; item["disc_number"] = 1; item["track_number"] = request.offset + n + 1
        }
        return item
    }
    let end = request.offset + count
    var object: [String: Any] = ["items": items, "total": total, "offset": request.offset, "limit": 50,
        "next_offset": end < total ? end as Any : NSNull(), "kind": request.category.rawValue,
        "album_uri": request.albumUri as Any? ?? NSNull(), "revision": revision, "coverage": "cached"]
    mutate(&object)
    return String(decoding: try JSONSerialization.data(withJSONObject: object), as: UTF8.self)
}

private actor CatalogRig {
    var requests: [LibraryBrowseRequest] = []
    var revision = "catalog:one"
    var total: UInt32 = 71
    var fail = false
    var hold = false
    var pending: CheckedContinuation<LibraryCatalogPage, Error>?
    var peak = 0
    var running = 0
    func read(_ request: LibraryBrowseRequest) async throws -> LibraryCatalogPage {
        requests.append(request); running += 1; peak = max(peak, running)
        defer { running -= 1 }
        if hold {
            hold = false
            return try await withCheckedThrowingContinuation { pending = $0 }
        }
        if fail { throw URLError(.notConnectedToInternet) }
        return try catalogPage(request, total: total, revision: revision)
    }
    func change(revision: String? = nil, total: UInt32? = nil, fail: Bool = false, hold: Bool = false) {
        if let revision { self.revision = revision }; if let total { self.total = total }
        self.fail = fail; self.hold = hold
    }
    func release() throws {
        guard let pending, let request = requests.last else { return }
        self.pending = nil
        pending.resume(returning: try catalogPage(request, total: total, revision: revision))
    }
    var count: Int { requests.count }
}

@MainActor private func drain(_ predicate: @MainActor () async -> Bool) async throws {
    for _ in 0..<2000 {
        if await predicate() { return }
        try await Task.sleep(for: .milliseconds(1))
    }
    throw URLError(.timedOut)
}
@MainActor private func loaded(_ rig: CatalogRig = CatalogRig()) async throws -> LibraryCatalogModel {
    let model = LibraryCatalogModel(searchDelay: .zero, now: { Date(timeIntervalSince1970: 100) }, read: { try await rig.read($0) })
    model.resume(); try await drain { !model.loading }
    #expect(model.isFresh)
    return model
}

@Suite("Native cached catalog contract") struct LibraryCatalogContractTests {
    @Test func queryContractHasNoDestinationOrImplicitSync() throws {
        let request = try LibraryBrowseRequest(category: .tracks, query: "  BWV 988 + piano  ", offset: 50,
                                               albumUri: "spotify:album:fixture")
        let fields = Dictionary(uniqueKeysWithValues: request.queryItems.map { ($0.name, $0.value!) })
        #expect(fields == ["kind": "tracks", "q": "BWV 988 + piano", "offset": "50", "limit": "50",
                           "album_uri": "spotify:album:fixture"])
        #expect(try LibraryBrowseRequest().queryItems.count == 3)
    }
    @Test func rejectsInvalidFiltersAndAlbumScopes() {
        for query in [String(repeating: "a", count: 513), String(repeating: "é", count: 257), "x\u{0000}y", "x\ny"] {
            #expect(throws: LibraryCatalogError.self) { try LibraryBrowseRequest(query: query) }
        }
        #expect(throws: LibraryCatalogError.self) { try LibraryBrowseRequest(category: .playlists, albumUri: "spotify:album:fixture") }
        for uri in ["spotify:track:a", "spotify:album:", "spotify:album:a/b", "spotify:album:å", "spotify:album:a?x=1"] {
            #expect(throws: LibraryCatalogError.self) { try LibraryBrowseRequest(category: .tracks, albumUri: uri) }
        }
    }
    @Test func acceptsEveryCategoryAndRetainsDuplicateTitlesAndExplicitFlags() throws {
        for category in LibraryCategory.allCases {
            let request = try LibraryBrowseRequest(category: category)
            let page = try catalogPage(request).validated(for: request)
            #expect(page.items.count == 50); #expect(page.nextOffset == 50)
            #expect(page.items[0].title == page.items[1].title)
            #expect(page.items[0].id != page.items[1].id)
            #expect(page.items[1].explicit)
        }
    }
    @Test func keepsAlbumDiscOrderAndPartialCoverage() throws {
        let request = try LibraryBrowseRequest(category: .tracks, albumUri: "spotify:album:fixture")
        let page = try catalogPage(request, total: 3).validated(for: request)
        #expect(page.coverage == "cached"); #expect(page.items.map(\.trackNumber) == [1, 2, 3])
        #expect(page.items.allSatisfy { $0.albumUri == request.albumUri })
    }
    @Test func emptyAndFinalPagesAreRealObservations() throws {
        let first = try LibraryBrowseRequest()
        #expect(try catalogPage(first, total: 0).validated(for: first).items.isEmpty)
        let last = try LibraryBrowseRequest(offset: 50)
        let page = try catalogPage(last, total: 54).validated(for: last)
        #expect(page.items.count == 4); #expect(page.nextOffset == nil)
        let gone = try LibraryBrowseRequest(offset: 100)
        #expect(try catalogPage(gone, total: 10).validated(for: gone).items.isEmpty)
    }
    @Test func mismatchedCountersScopeRevisionAndCoverageFailClosed() throws {
        let request = try LibraryBrowseRequest()
        let changes: [[String: Any]] = [["offset": 1], ["limit": 49], ["kind": "tracks"],
            ["album_uri": "spotify:album:other"], ["coverage": "complete"], ["revision": " "],
            ["revision": String(repeating: "x", count: 257)], ["next_offset": 49], ["items": []], ["total": 49]]
        for changes in changes {
            let page = try catalogPage(request) { value in value.merge(changes) { _, new in new } }
            #expect(throws: LibraryCatalogError.self) { try page.validated(for: request) }
        }
    }
    @Test func missingBadAndDuplicateIdentitiesCannotBecomePlaybackSources() throws {
        let request = try LibraryBrowseRequest()
        let changes: [[String: Any]] = [["source_uri": "spotify:track:wrong"], ["source_uri": "NOT_IMPLEMENTED"],
            ["source_uri": "https://example.invalid/music"], ["source_uri": "spotify:album:"],
            ["cached_tracks": 0], ["kind": "track"]]
        for change in changes {
            let page = try catalogPage(request) { value in
                var items = value["items"] as! [[String: Any]]; items[0].merge(change) { _, new in new }; value["items"] = items
            }
            #expect(throws: LibraryCatalogError.self) { try page.validated(for: request) }
        }
        let duplicate = try catalogPage(request) { value in
            var items = value["items"] as! [[String: Any]]; items[1] = items[0]; value["items"] = items
        }
        #expect(throws: LibraryCatalogError.self) { try duplicate.validated(for: request) }
    }
    @Test func wrongAlbumMembersAndMalformedScalarTypesFailClosed() throws {
        let request = try LibraryBrowseRequest(category: .tracks, albumUri: "spotify:album:fixture")
        let page = try catalogPage(request) { value in
            var items = value["items"] as! [[String: Any]]; items[0]["album_uri"] = "spotify:album:other"; value["items"] = items
        }
        #expect(throws: LibraryCatalogError.self) { try page.validated(for: request) }
        for field in ["total", "offset", "limit"] {
            #expect(throws: (any Error).self) { try catalogPage(request) { $0[field] = -1 } }
        }
        #expect(throws: (any Error).self) {
            try catalogPage(request) { value in
                var items = value["items"] as! [[String: Any]]; items[0]["explicit"] = "yes"; value["items"] = items
            }
        }
    }
}

@Suite("Native cached catalog lifecycle") @MainActor struct LibraryCatalogLifecycleTests {
    @Test func isLazyAndResumeIsIdempotent() async throws {
        let rig = CatalogRig(), model = LibraryCatalogModel(read: { try await rig.read($0) })
        #expect(await rig.count == 0)
        model.resume(); model.resume(); try await drain { !model.loading }
        #expect(await rig.count == 1); #expect(model.request.category == .albums)
        model.stop()
    }
    @Test func pagingKeepsOnlyOnePageAndOpaqueRevision() async throws {
        let rig = CatalogRig(), model = try await loaded(rig)
        let selected = try #require(model.selection(for: "spotify:album:fixture0"))
        model.nextPage(); #expect(model.resolve(selected) == nil)
        try await drain { !model.loading }
        #expect(model.page?.offset == 50); #expect(model.page?.items.count == 21)
        #expect(!model.canGoForward); #expect(model.canGoBack)
        model.previousPage(); try await drain { !model.loading }
        #expect(model.page?.offset == 0); #expect(model.page?.items.count == 50)
        model.stop()
    }
    @Test func changedRevisionRequiresExplicitRestart() async throws {
        let rig = CatalogRig(), model = try await loaded(rig)
        await rig.change(revision: "catalog:two")
        model.nextPage(); try await drain { !model.loading }
        #expect(model.restartRequired); #expect(!model.canChoose); #expect(model.page == nil)
        #expect(await rig.count == 2)
        model.nextPage(); #expect(await rig.count == 2)
        model.refresh(); try await drain { !model.loading }
        #expect(model.page?.offset == 0); #expect(model.page?.revision == "catalog:two")
        #expect(model.canChoose); model.stop()
    }
    @Test func albumDrillDownClearsTitleFilterAndBackRestoresIt() async throws {
        let rig = CatalogRig(), model = try await loaded(rig)
        model.filter("Same title"); try await drain { !model.loading }
        model.nextPage(); try await drain { !model.loading }
        let selected = try #require(model.selection(for: "spotify:album:fixture50"))
        model.openAlbum(selected); try await drain { !model.loading }
        #expect(model.request.category == .tracks); #expect(model.request.query.isEmpty)
        #expect(model.request.albumUri == selected.item.sourceUri); #expect(model.album?.title == "Same title")
        model.backToAlbums(); try await drain { !model.loading }
        #expect(model.request.category == .albums); #expect(model.request.query == "Same title")
        #expect(model.request.offset == 50); #expect(model.album == nil); model.stop()
    }
    @Test func oldAlbumSelectionCannotNavigateANewerPage() async throws {
        let model = try await loaded()
        let selected = try #require(model.selection(for: "spotify:album:fixture0"))
        model.refresh(); try await drain { !model.loading }
        model.openAlbum(selected)
        #expect(model.album == nil); #expect(model.request.category == .albums); model.stop()
    }
    @Test func failurePreservesObservationTimeButNotActionAuthority() async throws {
        let rig = CatalogRig(), model = try await loaded(rig)
        let at = model.observedAt, before = model.page
        await rig.change(fail: true); model.refresh(); try await drain { !model.loading }
        #expect(model.observedAt == at); #expect(model.page == before); #expect(!model.isFresh)
        #expect(model.selection(for: "spotify:album:fixture0") == nil)
        #expect(model.problem != nil); model.stop()
    }
    @Test func invalidQueryMakesRetainedRowsReadOnly() async throws {
        let model = try await loaded()
        model.filter(String(repeating: "x", count: 513))
        #expect(!model.canChoose); #expect(model.problem != nil)
        model.filter("recovered"); try await drain { !model.loading }
        #expect(model.canChoose); #expect(model.request.query == "recovered"); model.stop()
    }
    @Test func supersededReadDrainsAndOnlyLatestIntentStarts() async throws {
        let rig = CatalogRig(); await rig.change(hold: true)
        let model = LibraryCatalogModel(searchDelay: .zero, read: { try await rig.read($0) })
        model.resume(); try await drain { await rig.count == 1 }
        model.browse(.tracks); model.filter("old"); model.filter("new"); model.browse(.playlists)
        #expect(await rig.count == 1)
        try await rig.release(); try await drain { !model.loading }
        let requests = await rig.requests
        #expect(requests.count == 2); #expect(requests.last?.category == .playlists)
        #expect(requests.last?.query == "new"); #expect(model.page?.kind == .playlists)
        #expect(await rig.peak == 1); model.stop()
    }
    @Test func suspendFencesSlowRepliesAndDoesNotLaunchPendingWork() async throws {
        let rig = CatalogRig(); await rig.change(hold: true)
        let model = LibraryCatalogModel(searchDelay: .zero, read: { try await rig.read($0) })
        model.resume(); try await drain { await rig.count == 1 }
        model.filter("later"); model.suspend(); try await rig.release()
        try await drain { await rig.running == 0 }
        #expect(model.page == nil); #expect(await rig.count == 1); #expect(!model.canChoose)
        model.resume(); try await drain { !model.loading }
        #expect(model.request.query == "later"); #expect(model.canChoose); model.stop()
    }
    @Test func stopCannotBeResumedOrDispatchAHiddenRead() async throws {
        let rig = CatalogRig(), model = try await loaded(rig)
        model.stop(); model.resume(); model.refresh(); model.browse(.tracks); model.filter("hidden")
        #expect(!model.active); #expect(await rig.count == 1)
    }
    @Test func resumeCannotTreatUnobservedCachedPageAsFresh() async throws {
        let rig = CatalogRig(), model = try await loaded(rig)
        model.suspend(); await rig.change(hold: true); model.resume()
        #expect(!model.isFresh); #expect(!model.canChoose)
        try await drain { await rig.count == 2 }
        try await rig.release(); try await drain { !model.loading }
        #expect(model.canChoose); model.stop()
    }
    @Test func metadataRefreshRetiresOldSelectionEvenWhenRevisionMatches() async throws {
        let model = try await loaded()
        let selection = try #require(model.selection(for: "spotify:album:fixture0"))
        #expect(model.resolve(selection)?.sourceUri == selection.item.sourceUri)
        model.refresh(); #expect(model.resolve(selection) == nil)
        try await drain { !model.loading }
        #expect(model.resolve(selection) == nil)
        #expect(model.selection(for: selection.item.id) != nil); model.stop()
    }
    @Test func albumCacheChangingOnReturnDoesNotCombineListings() async throws {
        let rig = CatalogRig(), model = try await loaded(rig)
        model.openAlbum(try #require(model.selection(for: "spotify:album:fixture0")))
        try await drain { !model.loading }; await rig.change(revision: "catalog:updated")
        model.backToAlbums(); try await drain { !model.loading }
        #expect(model.restartRequired); #expect(!model.canChoose); model.stop()
    }
    @Test func failedCategoryReadCannotLeaveOldAlbumRowsInNewCategory() async throws {
        let rig = CatalogRig(), model = try await loaded(rig)
        await rig.change(fail: true); model.browse(.playlists); try await drain { !model.loading }
        #expect(model.request.category == .playlists); #expect(model.page == nil)
        #expect(model.observedAt == nil); #expect(!model.canChoose); model.stop()
    }
    @Test func malformedReaderReplyIsValidatedByProductionModel() async throws {
        let model = LibraryCatalogModel(read: { request in try catalogPage(request) { $0["coverage"] = "complete" } })
        model.resume(); try await drain { !model.loading }
        #expect(model.page == nil); #expect(!model.canChoose); #expect(model.problem != nil); model.stop()
    }
    @Test func rapidSearchCancelsDebounceWithoutSendingIntermediateFilters() async throws {
        let rig = CatalogRig(), model = LibraryCatalogModel(searchDelay: .milliseconds(20), read: { try await rig.read($0) })
        model.resume(); try await drain { !model.loading }
        model.filter("b"); model.filter("ba"); model.filter("bach")
        try await drain { !model.loading }
        let requests = await rig.requests
        #expect(requests.count == 2); #expect(requests.last?.query == "bach"); #expect(await rig.peak == 1)
        model.stop()
    }
    @Test func returningToTheOriginalFilterRecoversAfterInvalidInput() async throws {
        let model = try await loaded()
        model.filter(String(repeating: "x", count: 513)); #expect(!model.canChoose)
        model.filter(""); try await drain { !model.loading }
        #expect(model.canChoose); #expect(model.problem == nil); model.stop()
    }
    @Test func visibilityCannotClearARevisionRestartRequirement() async throws {
        let rig = CatalogRig(), model = try await loaded(rig)
        await rig.change(revision: "catalog:two")
        model.nextPage(); try await drain { !model.loading }; #expect(model.restartRequired)
        model.suspend(); model.resume()
        #expect(model.restartRequired); #expect(!model.canChoose); #expect(await rig.count == 2)
        model.refresh(); try await drain { !model.loading }
        #expect(model.page?.offset == 0); #expect(model.canChoose); model.stop()
    }
    @Test func interruptedPaginationRetainsItsExpectedRevision() async throws {
        let rig = CatalogRig(), model = try await loaded(rig)
        await rig.change(hold: true); model.nextPage()
        try await drain { await rig.count == 2 }
        model.suspend(); try await rig.release(); try await drain { await rig.running == 0 }
        await rig.change(revision: "catalog:two"); model.resume(); try await drain { !model.loading }
        #expect(model.restartRequired); #expect(!model.canChoose); model.stop()
    }
    @Test func invalidDraftCannotSilentlyRefreshAnEarlierFilter() async throws {
        let rig = CatalogRig(), model = try await loaded(rig)
        model.filter(String(repeating: "x", count: 513)); model.refresh()
        model.suspend(); model.resume()
        #expect(await rig.count == 1); #expect(!model.canChoose); #expect(model.problem != nil)
        model.filter(""); try await drain { !model.loading }
        #expect(model.canChoose); model.stop()
    }

}

@Suite("Native library browsing to reviewed playback") @MainActor struct LibraryCatalogPlaybackTests {
    @Test func productionBrowseAdapterEncodesFiltersAndKeepsMetadataLiteral() async throws {
        let request = try LibraryBrowseRequest(category: .tracks, query: "Bach & piano + 100% / # ?",
                                               offset: 50, albumUri: "spotify:album:fixture")
        let literal = #"<script>music & [a title](https://outside.example.test/art)</script>"#
        let transport = try RecordingTransport(catalogJSON(request, total: 51) { value in
            var items = value["items"] as! [[String: Any]]
            items[0]["title"] = literal; value["items"] = items
        })
        let page = try await client(transport).libraryCatalog(request)
        #expect(page.items.first?.title == literal)
        let requests = await transport.requests
        #expect(requests.count == 1)
        let sent = try #require(requests.first), url = try #require(sent.url)
        #expect(url.host == "music.example.test"); #expect(url.path == "/library/browse")
        #expect(sent.httpMethod == "GET"); #expect(sent.httpBody == nil)
        let fields = Dictionary(uniqueKeysWithValues: (URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems ?? []).map { ($0.name, $0.value) })
        #expect(fields == ["kind": "tracks", "offset": "50", "limit": "50", "q": request.query, "album_uri": request.albumUri])
        #expect(sent.value(forHTTPHeaderField: "Accept") == "application/json")
        #expect(sent.value(forHTTPHeaderField: "Authorization") == nil)
    }

    @Test func productionAdapterRejectsIncompleteServerPage() async throws {
        let request = try LibraryBrowseRequest()
        let transport = try RecordingTransport(catalogJSON(request) { $0["items"] = [] })
        await #expect(throws: LibraryCatalogError.invalidPage) { try await client(transport).libraryCatalog(request) }
        #expect(await transport.requests.count == 1)
    }

    @Test func browsingAndOpeningAlbumsNeedsNoRoomAndNeverWrites() async throws {
        let albums = try LibraryBrowseRequest()
        let tracks = try LibraryBrowseRequest(category: .tracks, albumUri: "spotify:album:fixture0")
        let playlists = try LibraryBrowseRequest(category: .playlists)
        let transport = try RecordingTransport(jsons: [catalogJSON(albums, total: 2),
                                                       catalogJSON(tracks, total: 4), catalogJSON(playlists, total: 3)])
        let model = try LibraryCatalogModel(client: client(transport), searchDelay: .zero)
        model.resume(); try await drain { !model.loading }
        #expect(model.canChoose)
        model.openAlbum(try #require(model.selection(for: "spotify:album:fixture0")))
        #expect(!model.canChooseAlbum)
        try await drain { !model.loading }
        #expect(model.canChooseAlbum); #expect(model.page?.items.count == 4)
        #expect(try model.command(forAlbum: #require(model.albumSelection()), action: .playNow) == .playSource("spotify:album:fixture0"))
        model.browse(.playlists); try await drain { !model.loading }
        #expect(model.page?.kind == .playlists); #expect(model.album == nil)
        let requests = await transport.requests
        #expect(requests.count == 3)
        #expect(requests.allSatisfy { $0.httpMethod == "GET" && $0.url?.path == "/library/browse" })
        #expect(requests.allSatisfy { !(URLComponents(url: $0.url!, resolvingAgainstBaseURL: false)?.queryItems ?? []).contains { $0.name == "zone" } })
        model.stop()
    }

    @Test(arguments: ["S1", "S2"], [LibraryCatalogAction.playNow, .queueNext, .queueLast])
    func catalogIntentUsesTheExistingOneUseHouseholdReview(household: String, action: LibraryCatalogAction) async throws {
        let request = try LibraryBrowseRequest(category: .playlists)
        let zones = zoneJSON.replacingOccurrences(of: "S2", with: household)
        let state = stateJSON.replacingOccurrences(of: "S2", with: household)
        let acknowledgement: String
        switch action {
        case .playNow: acknowledgement = #"{"done":"Playing","changed":true}"#
        case .queueNext, .queueLast: acknowledgement = #"{"done":"Queued","first":3,"count":4,"playing_queue":false}"#
        }
        // Initial browse, prepare reads, confirmation reads, and one write.
        let transport = try RecordingTransport(jsons: [catalogJSON(request, total: 1), "[\(zones)]", state,
                                                       "[\(zones)]", state, acknowledgement])
        let daemon = try client(transport)
        let catalog = LibraryCatalogModel(client: daemon, searchDelay: .zero)
        catalog.resume(); catalog.browse(.playlists); try await drain { !catalog.loading }
        let selection = try #require(catalog.selection(for: "spotify:playlist:fixture0"))
        let command = try catalog.command(for: selection, action: action)
        let session = CommandReviewSession(client: daemon)
        let zone = try fixture(Zone.self, zones)
        let review = try await session.prepare(command, in: zone)
        #expect(review.zone.id.address == "Studio@\(household)")
        #expect(await transport.requests.allSatisfy { $0.httpMethod == "GET" })
        _ = try await session.execute(review)
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        let requests = await transport.requests
        let writes = requests.filter { $0.httpMethod == "POST" }
        #expect(writes.count == 1)
        let sent = try #require(writes.first)
        let body = try JSONDecoder().decode([String: JSONValue].self, from: #require(sent.httpBody))
        #expect(body["zone"] == .string("Studio@\(household)"))
        #expect(body["source_uri"] == .string(selection.item.sourceUri))
        switch action {
        case .playNow: #expect(sent.url?.path == "/play"); #expect(body["next"] == nil)
        case .queueNext: #expect(sent.url?.path == "/queue"); #expect(body["next"] == .bool(true))
        case .queueLast: #expect(sent.url?.path == "/queue"); #expect(body["next"] == .bool(false))
        }
        #expect(requests.filter { $0.url?.path == "/zones" }.count == 2)
        catalog.stop()
    }

    @Test func expiredMenusCannotCreateACommandEvenForTheSameSource() async throws {
        let model = try await loaded()
        let selected = try #require(model.selection(for: "spotify:album:fixture0"))
        #expect(try model.command(for: selected, action: .queueNext) == .queueAdd(selected.item.sourceUri, next: true))
        model.refresh()
        #expect(throws: LibraryCatalogError.selectionExpired) { try model.command(for: selected, action: .playNow) }
        try await drain { !model.loading }
        #expect(model.selection(for: selected.item.id) != nil)
        for action in LibraryCatalogAction.allCases {
            #expect(throws: LibraryCatalogError.selectionExpired) { try model.command(for: selected, action: action) }
        }
        let current = try #require(model.selection(for: selected.item.id))
        model.suspend()
        #expect(throws: LibraryCatalogError.selectionExpired) { try model.command(for: current, action: .queueLast) }
        model.stop()
    }

    @Test func albumActionsRequireFreshScopedTracksAndRetireOnNavigation() async throws {
        let rig = CatalogRig(), model = try await loaded(rig)
        #expect(!model.canChooseAlbum)
        model.openAlbum(try #require(model.selection(for: "spotify:album:fixture0")))
        try await drain { !model.loading }
        let album = try #require(model.albumSelection())
        #expect(try model.command(forAlbum: album, action: .queueLast) == .queueAdd("spotify:album:fixture0", next: false))
        await rig.change(revision: "catalog:new")
        model.nextPage(); try await drain { !model.loading }
        #expect(model.restartRequired); #expect(!model.canChooseAlbum)
        #expect(throws: LibraryCatalogError.selectionExpired) { try model.command(forAlbum: album, action: .playNow) }
        model.refresh(); try await drain { !model.loading }
        #expect(model.canChooseAlbum)
        #expect(throws: LibraryCatalogError.selectionExpired) { try model.command(forAlbum: album, action: .queueNext) }
        let refreshedAlbum = try #require(model.albumSelection())
        #expect(try model.command(forAlbum: refreshedAlbum, action: .playNow) == .playSource(album.item.sourceUri))
        model.backToAlbums()
        #expect(!model.canChooseAlbum)
        #expect(throws: LibraryCatalogError.selectionExpired) { try model.command(forAlbum: refreshedAlbum, action: .queueNext) }
        model.stop()
    }

    @Test func invalidFilterMakesRefreshUnavailableUntilItsVisibleDraftIsCorrected() async throws {
        let rig = CatalogRig(), model = try await loaded(rig)
        #expect(model.canRefresh)
        model.filter(String(repeating: "x", count: 513))
        #expect(!model.canRefresh)
        model.suspend(); model.resume(); model.filter(model.request.query)
        try await drain { !model.loading }
        #expect(model.canRefresh); #expect(model.canChoose); #expect(await rig.count == 2)
        model.stop()
    }
}


private func completeAlbumPage(_ request: LibraryBrowseRequest,
                               mutate: (inout [String: Any]) -> Void = { _ in }) throws -> LibraryCatalogPage {
    try catalogPage(request, total: 3) { value in
        value["album_observation"] = ["coverage": "complete", "fetched_at": 1_700_000_000, "cached_total": 3]
        var items = value["items"] as! [[String: Any]]
        for index in items.indices {
            items[index]["source_uri"] = "spotify:track:Repeated"
            items[index]["explicit"] = false
        }
        items[1]["explicit_known"] = false
        items[1]["subtitle"] = ""
        items[2]["disc_number"] = 2
        items[2]["track_number"] = 1
        value["items"] = items
        mutate(&value)
    }
}

@Suite("Native completed album observations") struct CompletedAlbumCatalogTests {
    @Test func completedAlbumKeepsRepeatedSourcesAsDistinctOrderedOccurrences() throws {
        let request = try LibraryBrowseRequest(category: .tracks, albumUri: "spotify:album:fixture")
        let page = try completeAlbumPage(request).validated(for: request)
        #expect(page.coverage == "cached")
        #expect(page.albumObservation?.coverage == "complete")
        #expect(page.albumObservation?.fetchedAt == 1_700_000_000)
        #expect(page.albumObservation?.cachedTotal == 3)
        #expect(page.items.map(\.sourceUri) == Array(repeating: "spotify:track:Repeated", count: 3))
        #expect(Set(page.items.map(\.id)).count == 3)
        #expect(page.items.map(\.discNumber) == [1, 1, 2])
        #expect(page.items.map(\.trackNumber) == [1, 2, 1])
        #expect(page.items[0].explicitKnown)
        #expect(!page.items[1].explicitKnown)
        #expect(!page.items[1].explicit)
        #expect(try page.validated(for: request) == page)
    }

    @Test func olderDaemonsRetainURIIdentityAndKnownExplicitness() throws {
        let request = try LibraryBrowseRequest(category: .tracks, albumUri: "spotify:album:fixture")
        let page = try catalogPage(request, total: 3).validated(for: request)
        #expect(page.albumObservation == nil)
        #expect(page.items.allSatisfy { $0.id == $0.sourceUri && $0.explicitKnown })
        let subset = try catalogPage(request, total: 3) {
            $0["album_observation"] = ["coverage": "library_subset", "cached_total": 3]
        }.validated(for: request)
        #expect(subset.albumObservation?.fetchedAt == nil)
        #expect(subset.items.allSatisfy { $0.id == $0.sourceUri })
        let duplicate = try catalogPage(request, total: 3) { value in
            var items = value["items"] as! [[String: Any]]
            items[1]["source_uri"] = items[0]["source_uri"]
            value["items"] = items
        }
        #expect(throws: LibraryCatalogError.self) { try duplicate.validated(for: request) }
    }

    @Test func inconsistentCoverageAndUnknownExplicitClaimsAreRejected() throws {
        let request = try LibraryBrowseRequest(category: .tracks, albumUri: "spotify:album:fixture")
        let observations: [[String: Any]] = [
            ["coverage": "complete", "cached_total": 3],
            ["coverage": "complete", "fetched_at": -1, "cached_total": 3],
            ["coverage": "complete", "fetched_at": 8_640_000_000_001 as Int64, "cached_total": 3],
            ["coverage": "complete", "fetched_at": 1, "cached_total": 2],
            ["coverage": "library_subset", "fetched_at": 1, "cached_total": 3],
            ["coverage": "unknown", "cached_total": 3]
        ]
        for observation in observations {
            let page = try completeAlbumPage(request) { $0["album_observation"] = observation }
            #expect(throws: LibraryCatalogError.self) { try page.validated(for: request) }
        }
        let contradictory = try completeAlbumPage(request) { value in
            var items = value["items"] as! [[String: Any]]
            items[1]["explicit"] = true; value["items"] = items
        }
        #expect(throws: LibraryCatalogError.self) { try contradictory.validated(for: request) }
        let unscoped = try LibraryBrowseRequest(category: .tracks)
        let outsideAlbum = try catalogPage(unscoped, total: 1) {
            $0["album_observation"] = ["coverage": "complete", "fetched_at": 1, "cached_total": 1]
        }
        #expect(throws: LibraryCatalogError.self) { try outsideAlbum.validated(for: unscoped) }
    }

    @Test func missingRepeatedAndReversedPositionsCannotBecomeSelections() throws {
        let request = try LibraryBrowseRequest(category: .tracks, albumUri: "spotify:album:fixture")
        for mutation in 0..<4 {
            let page = try completeAlbumPage(request) { value in
                var items = value["items"] as! [[String: Any]]
                switch mutation {
                case 0: items[1].removeValue(forKey: "track_number")
                case 1: items[1]["disc_number"] = 0
                case 2: items[1]["track_number"] = 1
                default: items.reverse()
                }
                value["items"] = items
            }
            #expect(throws: LibraryCatalogError.self) { try page.validated(for: request) }
        }
    }

    @Test @MainActor func repeatedOccurrencesUseExactSourcesAndExpireOnRefresh() async throws {
        let model = LibraryCatalogModel(searchDelay: .zero, read: { request in
            if request.albumUri != nil { return try completeAlbumPage(request) }
            return try catalogPage(request, total: 1)
        })
        model.resume(); try await drain { !model.loading }
        model.openAlbum(try #require(model.selection(for: "spotify:album:fixture0")))
        try await drain { !model.loading }
        let items = try #require(model.page?.items)
        let first = try #require(model.selection(for: items[0].id))
        let repeated = try #require(model.selection(for: items[1].id))
        #expect(first != repeated)
        #expect(try model.command(for: first, action: .playNow) == .playSource("spotify:track:Repeated"))
        #expect(try model.command(for: repeated, action: .queueNext) == .queueAdd("spotify:track:Repeated", next: true))
        #expect(model.selection(for: "spotify:track:Repeated") == nil)
        model.refresh()
        #expect(throws: LibraryCatalogError.self) { try model.command(for: repeated, action: .queueNext) }
        try await drain { !model.loading }
        #expect(throws: LibraryCatalogError.self) { try model.command(for: first, action: .playNow) }
        model.stop()
    }
}
