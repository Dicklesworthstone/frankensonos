import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

private let paginationFirstID = "0123456789ABCDEFGHIJKL"
private let paginationLaterID = "abcdefghijklmnopqrstuv"

private func paginationItem(_ kind: SpotifyCatalogKind = .track, later: Bool = false) -> SpotifySearchItem {
    SpotifySearchItem(sourceUri: "spotify:\(kind.rawValue):\(later ? paginationLaterID : paginationFirstID)",
                      title: later ? "Later source" : "First source", subtitle: nil)
}

private func paginationBody(offset: UInt32 = 0, next: UInt32? = nil, more: Bool = false) -> [String: Any] {
    var body: [String: Any] = [
        "query": "Music & guests", "kind": "track", "offset": offset, "more": more,
        "items": [["source_uri": "spotify:track:\(paginationFirstID)", "title": "A result"]]
    ]
    body["next_offset"] = next.map { $0 as Any } ?? NSNull()
    return body
}

private func paginationDecode(_ body: [String: Any]) throws -> SpotifySearchPage {
    try DaemonClient.decode(SpotifySearchPage.self, from: JSONSerialization.data(withJSONObject: body))
}

@Suite("Native Spotify search pagination contract") struct SpotifySearchPaginationTests {
    @Test func offsetsPreserveTheExactQueryKindAndLimit() throws {
        for offset in [UInt32(0), 3, 1_000] {
            let request = try SpotifySearchRequest(query: "  Music & guests  ", kind: .album, limit: 3, offset: offset)
            #expect(request.query == "Music & guests" && request.kind == .album && request.limit == 3)
            #expect(request.offset == offset)
            var expected = [URLQueryItem(name: "q", value: "Music & guests"),
                            URLQueryItem(name: "kind", value: "album"), URLQueryItem(name: "limit", value: "3")]
            if offset != 0 { expected.append(URLQueryItem(name: "offset", value: String(offset))) }
            #expect(request.queryItems == expected)
        }
        for offset in [UInt32(1_001), .max] {
            #expect(throws: ContractError.self) {
                try SpotifySearchRequest(query: "Music & guests", offset: offset)
            }
        }
    }

    @Test func oldFirstPageResponsesRemainReadableWithoutInventingContinuation() throws {
        var body = paginationBody(more: true)
        body.removeValue(forKey: "offset"); body.removeValue(forKey: "next_offset")
        let page = try paginationDecode(body)
        #expect(try page.validated(for: SpotifySearchRequest(query: "Music & guests")).offset == 0)
        #expect(page.more && page.nextOffset == nil)
        #expect(throws: ContractError.self) {
            try page.validated(for: SpotifySearchRequest(query: "Music & guests", offset: 3))
        }
        // The old memberwise initializer remains source-compatible.
        let old = SpotifySearchPage(query: page.query, kind: page.kind, items: page.items, more: true)
        #expect(old.offset == 0 && old.nextOffset == nil)
    }

    @Test func rawProgressCanCrossFilteredRowsAndStopAtTheProviderBound() throws {
        let request = try SpotifySearchRequest(query: "Music & guests", limit: 3, offset: 7)
        let page = try paginationDecode(paginationBody(offset: 7, next: 10, more: true)).validated(for: request)
        #expect(page.items.count == 1 && page.nextOffset == 10)
        var empty = paginationBody(offset: 7, next: 10, more: true)
        empty["items"] = []
        #expect(try paginationDecode(empty).validated(for: request).nextOffset == 10)
        let last = try paginationDecode(paginationBody(offset: 1_000, more: true))
            .validated(for: SpotifySearchRequest(query: "Music & guests", offset: 1_000))
        #expect(last.more && last.nextOffset == nil)
    }

    @Test func malformedPagingFieldsCannotOfferSources() throws {
        let request = try SpotifySearchRequest(query: "Music & guests", limit: 3, offset: 7)
        let mutations: [(inout [String: Any]) -> Void] = [
            { $0["offset"] = 0 }, { $0["offset"] = NSNull() }, { $0["offset"] = -1 },
            { $0["offset"] = "7" }, { $0["offset"] = 7.5 },
            { $0["next_offset"] = 7 }, { $0["next_offset"] = 6 }, { $0["next_offset"] = 11 },
            { $0["next_offset"] = 1_001 }, { $0["next_offset"] = -1 },
            { $0["next_offset"] = "10" }, { $0["next_offset"] = 9.5 },
            { $0["more"] = false }, { $0["query"] = "Another search" }, { $0["kind"] = "album" },
            {
                $0["next_offset"] = 8
                $0["items"] = [paginationFirstID, paginationLaterID].map {
                    ["source_uri": "spotify:track:\($0)", "title": "A result"]
                }
            }
        ]
        for mutation in mutations {
            var body = paginationBody(offset: 7, next: 10, more: true)
            mutation(&body)
            #expect(throws: (any Error).self) { try paginationDecode(body).validated(for: request) }
        }
    }

    @Test @MainActor func daemonPagingReadsOnlyTheSelectedDaemonAndKeepsTheChosenLaterSource() async throws {
        let transport = PaginationTransport()
        let client = DaemonClient(endpoint: try DaemonEndpoint("https://music.example.test"), transport: transport)
        let model = SpotifySearchModel(client: client)
        defer { model.stop() }
        model.resume(); model.edit("Music & guests"); model.chooseKind(.album); model.search()
        try await paginationSettled(model)
        model.nextPage(); try await paginationSettled(model)
        let observed = try #require(model.observation)
        #expect(observed.request.offset == 7 && observed.page.offset == 7)
        let item = try #require(observed.page.items.first)
        let zone = Zone(household: "S2", coordinatorRoom: "Studio", members: ["Studio"], transportState: "paused")
        let selected = try model.selection(item, from: observed.id, in: zone, action: .queueNext)
        #expect(selected.command == .queueAdd("spotify:album:\(paginationLaterID)", next: true))
        let requests = await transport.requests
        #expect(requests.count == 2)
        for request in requests {
            #expect(request.httpMethod == "GET" && request.httpBody == nil && request.timeoutInterval == 60)
            #expect(request.url?.host == "music.example.test" && request.url?.path == "/spotify/search")
            let parameters = try #require(URLComponents(url: request.url!, resolvingAgainstBaseURL: false)?.queryItems)
            #expect(parameters.first { $0.name == "q" }?.value == "Music & guests")
            #expect(parameters.first { $0.name == "kind" }?.value == "album")
            #expect(parameters.first { $0.name == "limit" }?.value == "10")
        }
        let later = URLComponents(url: requests[1].url!, resolvingAgainstBaseURL: false)?.queryItems
        #expect(later?.first { $0.name == "offset" }?.value == "7")
    }
}

private actor PaginationTransport: DaemonTransport {
    var requests: [URLRequest] = []
    func send(_ request: URLRequest) async throws -> HTTPReply {
        requests.append(request)
        guard request.httpMethod == "GET", request.httpBody == nil, request.url?.path == "/spotify/search",
              let url = request.url,
              let parameters = URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems else {
            throw ContractError.invalid("Pagination must only read the catalog")
        }
        let offset = UInt32(parameters.first { $0.name == "offset" }?.value ?? "0")!
        let kind = parameters.first { $0.name == "kind" }!.value!
        var body = paginationBody(offset: offset, next: offset == 0 ? 7 : nil, more: offset == 0)
        body["query"] = parameters.first { $0.name == "q" }!.value!
        body["kind"] = kind
        body["items"] = [["source_uri": "spotify:\(kind):\(offset == 0 ? paginationFirstID : paginationLaterID)",
                           "title": "Selected result"]]
        return HTTPReply(data: try JSONSerialization.data(withJSONObject: body), status: 200)
    }
    nonisolated func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> {
        AsyncThrowingStream { $0.finish() }
    }
}

private actor PaginationReader {
    var requests: [SpotifySearchRequest] = []
    var active = 0, maximumActive = 0
    var waiting = false
    private var hold = false, stale = false, legacy = false
    private var gate: CheckedContinuation<Void, Never>?
    func holdNext() { hold = true }
    func returnStaleFirstPage() { stale = true }
    func returnLegacyFirstPage() { legacy = true }
    func release() { let old = gate; gate = nil; waiting = false; old?.resume() }
    func read(_ request: SpotifySearchRequest) async throws -> SpotifySearchPage {
        requests.append(request); active += 1; maximumActive = max(active, maximumActive)
        defer { active -= 1 }
        if hold {
            hold = false; waiting = true
            await withCheckedContinuation { gate = $0 }
        }
        let offset = stale ? 0 : request.offset
        let next: UInt32? = legacy ? nil : offset == 0 ? 3 : offset == 3 ? 7 : nil
        return SpotifySearchPage(query: request.query, kind: request.kind,
                                 items: offset == 3 ? [] : [paginationItem(request.kind, later: offset != 0)],
                                 more: legacy || next != nil, offset: offset, nextOffset: next)
    }
}

@MainActor private func paginationSettled(_ model: SpotifySearchModel) async throws {
    let deadline = ContinuousClock.now.advanced(by: .seconds(3))
    while model.loading {
        guard ContinuousClock.now < deadline else { throw URLError(.timedOut) }
        try await Task.sleep(for: .milliseconds(2))
    }
}
private func paginationWaiting(_ reader: PaginationReader) async throws {
    let deadline = ContinuousClock.now.advanced(by: .seconds(3))
    while !(await reader.waiting) {
        guard ContinuousClock.now < deadline else { throw URLError(.timedOut) }
        try await Task.sleep(for: .milliseconds(2))
    }
}

@Suite("Native Spotify search pagination lifecycle") @MainActor struct SpotifySearchPaginationModelTests {
    @Test func nextAndPreviousUseRawOffsetsIncludingEmptyFilteredPages() async throws {
        let reader = PaginationReader(), model = SpotifySearchModel(read: { try await reader.read($0) })
        defer { model.stop() }
        model.resume(); model.edit("Music & guests"); model.chooseKind(.artist); model.search()
        try await paginationSettled(model)
        let first = try #require(model.observation)
        #expect(model.canNextPage && !model.canPreviousPage)
        model.nextPage()
        #expect(!model.canChoose && !model.canNextPage && !model.canPreviousPage)
        try await paginationSettled(model)
        #expect(model.observation?.page.offset == 3 && model.observation?.page.items.isEmpty == true)
        #expect(model.canNextPage && model.canPreviousPage)
        let zone = Zone(household: "S2", coordinatorRoom: "Studio", members: ["Studio"], transportState: "paused")
        #expect(throws: ContractError.self) {
            try model.selection(first.page.items[0], from: first.id, in: zone, action: .playNow)
        }
        model.nextPage(); try await paginationSettled(model)
        #expect(model.observation?.page.offset == 7 && !model.canNextPage && model.canPreviousPage)
        model.previousPage(); try await paginationSettled(model)
        #expect(model.observation?.page.offset == 3 && model.canPreviousPage)
        model.previousPage(); try await paginationSettled(model)
        #expect(model.observation?.page.offset == 0 && !model.canPreviousPage)
        let requests = await reader.requests
        #expect(requests.map(\.offset) == [0, 3, 7, 3, 0])
        #expect(requests.allSatisfy { $0.query == "Music & guests" && $0.kind == .artist && $0.limit == 10 })
    }

    @Test func newSearchAndKindChangesRestartAtZero() async throws {
        let reader = PaginationReader(), model = SpotifySearchModel(read: { try await reader.read($0) })
        defer { model.stop() }
        model.resume(); model.edit("Music & guests"); model.search(); try await paginationSettled(model)
        model.nextPage(); try await paginationSettled(model)
        model.search(); try await paginationSettled(model)
        #expect(model.observation?.page.offset == 0 && !model.canPreviousPage)
        model.nextPage(); try await paginationSettled(model)
        model.chooseKind(.show)
        #expect(!model.canNextPage && !model.canPreviousPage && !model.canChoose)
        model.search(); try await paginationSettled(model)
        #expect(model.observation?.request.kind == .show && model.observation?.page.offset == 0)
        #expect(!model.canPreviousPage)
    }

    @Test(arguments: ["edit", "kind", "cancel", "suspend", "stop"])
    func pagingCancellationDrainsWithoutRevivingSourcesOrStartingAnotherRead(operation: String) async throws {
        let reader = PaginationReader(), model = SpotifySearchModel(read: { try await reader.read($0) })
        defer { model.stop() }
        model.resume(); model.edit("Music & guests"); model.search(); try await paginationSettled(model)
        let first = try #require(model.observation)
        await reader.holdNext(); model.nextPage(); try await paginationWaiting(reader)
        model.nextPage(); model.previousPage(); model.search()
        #expect(await reader.requests.count == 2)
        switch operation {
        case "edit": model.edit("Another search")
        case "kind": model.chooseKind(.episode)
        case "cancel": model.cancel()
        case "suspend": model.suspend()
        default: model.stop()
        }
        model.resume(); model.search()
        #expect(model.loading && !model.canChoose && !model.canNextPage && !model.canPreviousPage)
        await reader.release(); try await paginationSettled(model)
        #expect(!model.isCurrent && !model.canChoose && !model.canNextPage && !model.canPreviousPage)
        #expect(operation == "stop" ? model.observation == nil : model.observation?.id == first.id)
        #expect(await reader.requests.map(\.offset) == [0, 3])
        #expect(await reader.maximumActive == 1)
    }

    @Test func staleFirstPageCannotCompleteAContinuationOrRestoreItsSelection() async throws {
        let reader = PaginationReader(), model = SpotifySearchModel(read: { try await reader.read($0) })
        defer { model.stop() }
        model.resume(); model.edit("Music & guests"); model.search(); try await paginationSettled(model)
        let first = try #require(model.observation)
        await reader.returnStaleFirstPage(); model.nextPage(); try await paginationSettled(model)
        #expect(model.problem != nil && !model.canChoose && !model.canNextPage && !model.canPreviousPage)
        #expect(model.observation?.id == first.id && !model.isCurrent)
        #expect(await reader.requests.count == 2)
    }

    @Test func legacyMoreFlagDoesNotGuessAnotherPage() async throws {
        let reader = PaginationReader(), model = SpotifySearchModel(read: { try await reader.read($0) })
        defer { model.stop() }
        await reader.returnLegacyFirstPage()
        model.resume(); model.edit("Music & guests"); model.search(); try await paginationSettled(model)
        #expect(model.canChoose && !model.canNextPage && !model.canPreviousPage)
        model.nextPage(); model.previousPage()
        #expect(await reader.requests.count == 1)
    }
}
