import Foundation
import Testing
@testable import FrankenSonosKit

private func catalogPage(_ request: LibraryBrowseRequest, total: UInt32 = 71,
                         revision: String = "catalog:one", mutate: (inout [String: Any]) -> Void = { _ in }) throws -> LibraryCatalogPage {
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
    let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
    return try decoder.decode(LibraryCatalogPage.self, from: JSONSerialization.data(withJSONObject: object))
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
