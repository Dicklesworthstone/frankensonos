import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

private func searchZone(_ house: String = "S2", room: String = "Den") -> Zone {
    Zone(household: house, coordinatorRoom: room, members: [room], transportState: "playing")
}
private func searchItem(_ position: UInt32, uri: String? = "fixture:duplicate") -> QueueItem {
    QueueItem(position: position, title: "Dvořák + friends", uri: uri, creator: "Artist", album: "Album")
}
private func searchPage(_ request: QueueSearchRequest, total: UInt32 = 100, examined: UInt32? = nil,
                        next: UInt32? = nil, items: [QueueItem] = [], revision: String = "rev:1") -> QueueSearchPage {
    let examined = examined ?? (total > request.offset ? total - request.offset : 0)
    return QueueSearchPage(zone: request.zone.coordinatorRoom, query: request.query, total: total,
                           revision: revision, offset: request.offset, examined: examined,
                           nextOffset: next, complete: next == nil, items: items)
}

@Test func queueSearchRequestKeepsExactQueryAndContinuationWithoutFloatingPoint() throws {
    let request = try QueueSearchRequest(in: searchZone("S1", room: "Den + / %"),
                                        query: "  Dvořák + friends  ", offset: UInt32.max - 1,
                                        limit: 200, expectedRevision: "opaque+/=%")
    #expect(request.query == "Dvořák + friends")
    #expect(request.queryItems.map(\.name) == ["q", "offset", "limit", "expected_revision"])
    #expect(request.queryItems[1].value == "4294967294")
    #expect(request.expectedRevision == "opaque+/=%")
    for bad in ["", "   ", "---", "line\nfeed", String(repeating: "é", count: 513),
                Array(repeating: "word", count: 33).joined(separator: " ")] {
        #expect(throws: ContractError.self) { try QueueSearchRequest(in: searchZone(), query: bad) }
    }
    #expect(throws: ContractError.self) { try QueueSearchRequest(in: searchZone(), query: "a", offset: 1) }
    #expect(throws: ContractError.self) { try QueueSearchRequest(in: searchZone(), query: "a", limit: 0) }
    #expect(throws: ContractError.self) { try QueueSearchRequest(in: searchZone(), query: "a", limit: 201) }
    #expect(throws: ContractError.self) { try QueueSearchRequest(in: searchZone(), query: "a", expectedRevision: "\n") }
}

@Test func queueSearchKeepsSparseAndRepeatedOccurrencesAndExactCoverage() throws {
    let request = try QueueSearchRequest(in: searchZone(), query: "Dvorak")
    let page = searchPage(request, total: 100_000, examined: 9_900, next: 9_900,
                          items: [searchItem(3), searchItem(129), searchItem(9_900, uri: nil)])
    let checked = try page.validated(for: request)
    #expect(checked.items.map(\.position) == [3, 129, 9_900])
    #expect(checked.items[0].uri == checked.items[1].uri)
    #expect(!checked.complete && checked.coverage.contains("9900"))
    #expect(!checked.items[2].playable)
    let empty = searchPage(request, total: 0)
    #expect(try empty.validated(for: request).complete)
    let continued = try QueueSearchRequest(in: searchZone(), query: "Dvorak", offset: 100, expectedRevision: "rev:1")
    #expect(try searchPage(continued, total: 100).validated(for: continued).examined == 0)
}

@Test func queueSearchRejectsContradictoryIncompleteAndUnboundedReplies() throws {
    let request = try QueueSearchRequest(in: searchZone(), query: "a", limit: 2)
    let valid: [String: Any] = ["zone": "Den", "query": "a", "total": 100, "revision": "rev:1",
                               "offset": 0, "examined": 50, "next_offset": 50, "complete": false,
                               "items": [["position": 3, "title": "a", "uri": "fixture:1"]]]
    for (field, value) in [
        ("zone", "Elsewhere" as Any), ("query", "b"), ("offset", 1), ("examined", 0),
        ("examined", 9_901), ("next_offset", 49), ("complete", true), ("revision", ""),
        ("total", 49), ("examined", -1), ("total", 1.5), ("complete", "false"),
        ("items", [["position": 51, "title": "a"]]),
        ("items", [["position": 3, "title": "a"], ["position": 3, "title": "b"]]),
        ("items", [["position": 4, "title": "a"], ["position": 3, "title": "b"]]),
        ("items", [["position": 3, "title": String(repeating: "x", count: 16_385)]])
    ] {
        var body = valid; body[field] = value
        let data = try JSONSerialization.data(withJSONObject: body)
        #expect(throws: (any Error).self) { try DaemonClient.decode(QueueSearchPage.self, from: data).validated(for: request) }
    }
    for field in ["total", "items", "complete", "examined", "revision", "next_offset"] {
        var body = valid; body.removeValue(forKey: field)
        let data = try JSONSerialization.data(withJSONObject: body)
        #expect(throws: (any Error).self) { try DaemonClient.decode(QueueSearchPage.self, from: data).validated(for: request) }
    }
    let continuation = try QueueSearchRequest(in: searchZone(), query: "a", offset: 50, expectedRevision: "old")
    #expect(throws: ContractError.self) { try searchPage(continuation).validated(for: continuation) }
}

private actor SearchTransport: DaemonTransport {
    var requests: [URLRequest] = []
    var replies: [HTTPReply]
    var held: CheckedContinuation<Void, Never>?
    var holdPath: String?
    init(_ replies: [HTTPReply]) { self.replies = replies }
    func block(_ path: String) { holdPath = path }
    func release() { held?.resume(); held = nil }
    func send(_ request: URLRequest) async throws -> HTTPReply {
        requests.append(request)
        if request.url?.path == holdPath {
            holdPath = nil
            await withCheckedContinuation { held = $0 }
        }
        guard !replies.isEmpty else { throw ContractError.invalid("Unexpected request") }
        return replies.removeFirst()
    }
    nonisolated func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> {
        AsyncThrowingStream { $0.finish() }
    }
}
private func topologyReply(_ zones: [Zone]) throws -> HTTPReply {
    let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
    return HTTPReply(data: try encoder.encode(zones), status: 200)
}
private func queueSearchReply(_ query: String = "a", room: String = "Den", status: Int = 200) throws -> HTTPReply {
    HTTPReply(data: try JSONSerialization.data(withJSONObject: ["zone": room, "query": query, "total": 10,
        "revision": "rev:1", "offset": 0, "examined": 10, "complete": true,
        "items": [["position": 7, "title": "a", "uri": "fixture:1"]]]), status: status)
}
private func queueSearchClient(_ transport: SearchTransport) throws -> DaemonClient {
    DaemonClient(endpoint: try DaemonEndpoint("https://music.example.test"), transport: transport)
}
private func searchEventually(_ condition: @Sendable () async -> Bool) async throws {
    let until = ContinuousClock.now.advanced(by: .seconds(3))
    while !(await condition()) {
        guard ContinuousClock.now < until else { throw ContractError.invalid("Search did not settle") }
        try await Task.sleep(for: .milliseconds(5))
    }
}

@Test(arguments: ["S1", "S2"]) func queueSearchClientUsesOneScopedGETAfterTopology(household: String) async throws {
    let zone = searchZone(household, room: "Den + / %")
    let transport = SearchTransport([try topologyReply([zone]), try queueSearchReply("a + b", room: zone.coordinatorRoom)])
    let page = try await queueSearchClient(transport).searchQueue(QueueSearchRequest(in: zone, query: "a + b"))
    let requests = await transport.requests
    #expect(page.items.first?.position == 7)
    #expect(requests.count == 2 && requests.allSatisfy { $0.httpMethod == "GET" && $0.httpBody == nil })
    #expect(requests[0].url?.path == "/zones")
    #expect(requests[1].url?.absoluteString.contains("Den%20%2B%20%2F%20%25%40\(household)/queue/search") == true)
    #expect(requests[1].url?.absoluteString.contains("q=a%20%2B%20b") == true)
    #expect(requests[1].timeoutInterval == 120)
    #expect(requests[1].value(forHTTPHeaderField: "Accept") == "application/json")
    #expect(requests[1].value(forHTTPHeaderField: "Authorization") == nil)
}

@Test func queueSearchClientRefusesDifferentHouseholdOrMembershipBeforeScan() async throws {
    for zones in [[], [searchZone("S1")], [Zone(household: "S2", coordinatorRoom: "Den", members: ["Den", "Hall"], transportState: "playing")]] {
        let transport = SearchTransport([try topologyReply(zones)])
        await #expect(throws: ContractError.self) {
            try await queueSearchClient(transport).searchQueue(QueueSearchRequest(in: searchZone(), query: "a"))
        }
        #expect(await transport.requests.count == 1)
    }
}

@Test func queueSearchErrorsStayReadOnlyAndAreNotRetried() async throws {
    let request = try QueueSearchRequest(in: searchZone(), query: "a")
    let problem = HTTPReply(data: Data(#"{"code":"QUEUE_CHANGED","detail":"Changed","hint":"Start again"}"#.utf8), status: 409)
    let transport = SearchTransport([try topologyReply([request.zone]), problem])
    do { _ = try await queueSearchClient(transport).searchQueue(request); Issue.record("Unexpected success") }
    catch let error as APIProblem { #expect(error.code == "QUEUE_CHANGED" && error.hint == "Start again") }
    #expect(await transport.requests.count == 2)
    let malformed = SearchTransport([try topologyReply([request.zone]), HTTPReply(data: Data("{}".utf8), status: 200)])
    await #expect(throws: (any Error).self) { try await queueSearchClient(malformed).searchQueue(request) }
    #expect(await malformed.requests.count == 2)
}

@Test func queueSearchCancellationStopsBeforeScanAndDiscardsLateReplies() async throws {
    for path in ["/zones", "/zones/Den@S2/queue/search"] {
        let request = try QueueSearchRequest(in: searchZone(), query: "a")
        let transport = SearchTransport([try topologyReply([request.zone]), try queueSearchReply()])
        await transport.block(path)
        let client = try queueSearchClient(transport)
        let work = Task { try await client.searchQueue(request) }
        try await searchEventually { await transport.held != nil }
        work.cancel(); await transport.release()
        await #expect(throws: CancellationError.self) { try await work.value }
        #expect(await transport.requests.count == (path == "/zones" ? 1 : 2))
    }
}

private actor SearchReader {
    var requests: [QueueSearchRequest] = []
    var gate: CheckedContinuation<Void, Never>?
    var blocked = false
    var failure = false
    var total: UInt32 = 6
    func hold() { blocked = true }
    func release() { gate?.resume(); gate = nil; blocked = false }
    func fail() { failure = true }
    func changeTotal() { total = 7 }
    func read(_ request: QueueSearchRequest) async throws -> QueueSearchPage {
        requests.append(request)
        if blocked { await withCheckedContinuation { gate = $0 } }
        if failure { throw ContractError.invalid("Unavailable") }
        let end = min(total, request.offset + 3)
        return searchPage(request, total: total, examined: end - request.offset, next: end < total ? end : nil,
                           items: [searchItem(request.offset + 1), searchItem(end)])
    }
}

@Test @MainActor func queueSearchIsExplicitAndContinuationReplacesRatherThanAccumulates() async throws {
    let reader = SearchReader(), model = QueueSearchModel(read: { try await reader.read($0) })
    defer { model.stop() }
    model.activate(in: searchZone()); model.edit("a")
    #expect(await reader.requests.isEmpty)
    model.search(); model.search(); model.continueSearch()
    try await searchEventually { await MainActor.run { model.isCurrent && !model.loading } }
    let first = try #require(model.observation)
    #expect(first.page.items.map(\.position) == [1, 3] && model.canContinue)
    let selection = try model.selection(first.page.items[1], from: first.id)
    #expect(selection.zone.household == "S2")
    #expect(selection.command == .queuePlay(first.page.items[1], revision: "rev:1"))
    model.continueSearch()
    try await searchEventually { await MainActor.run { model.observation?.id != first.id && !model.loading } }
    #expect(model.observation?.page.items.map(\.position) == [4, 6])
    #expect(!model.canContinue)
    let calls = await reader.requests
    #expect(calls.count == 2 && calls[1].offset == 3 && calls[1].expectedRevision == "rev:1")
    #expect(throws: ContractError.self) { try model.selection(first.page.items[0], from: first.id) }
}

@Test @MainActor func queueSearchEditsInvalidationsAndVisibilityRetireSelectionsWithoutReads() async throws {
    let reader = SearchReader(), model = QueueSearchModel(read: { try await reader.read($0) })
    defer { model.stop() }
    model.activate(in: searchZone()); model.edit("a"); model.search()
    try await searchEventually { await MainActor.run { model.isCurrent && !model.loading } }
    let observed = try #require(model.observation)
    model.activate(in: Zone(household: "S2", coordinatorRoom: "Den", members: ["Den"], transportState: "paused"))
    #expect(model.isCurrent)
    model.edit("b"); model.edit("a")
    #expect(!model.isCurrent && !model.canContinue)
    #expect(throws: ContractError.self) { try model.selection(observed.page.items[0], from: observed.id) }
    model.suspend(); model.activate(in: searchZone())
    #expect(!model.isCurrent && model.observation != nil)
    model.invalidate()
    #expect(await reader.requests.count == 1)
    model.stop(); model.activate(in: searchZone()); model.search()
    #expect(!model.active && model.observation == nil)
    #expect(await reader.requests.count == 1)
}

@Test @MainActor func queueSearchCancellationDrainsAndCannotPublishAcrossHouseholds() async throws {
    let reader = SearchReader(), model = QueueSearchModel(read: { try await reader.read($0) })
    defer { model.stop() }
    await reader.hold()
    model.activate(in: searchZone()); model.edit("a"); model.search()
    try await searchEventually { await reader.gate != nil }
    model.activate(in: searchZone("S1")); model.search()
    #expect(model.loading && !model.canSearch)
    await reader.release()
    try await searchEventually { await MainActor.run { !model.loading } }
    #expect(model.observation == nil && model.canSearch)
    #expect(await reader.requests.count == 1)
    model.search()
    try await searchEventually { await MainActor.run { model.isCurrent } }
    #expect(model.observation?.request.zone.household == "S1")
}

@Test @MainActor func queueSearchImmediateCancellationNeverInvokesReader() async throws {
    let reader = SearchReader(), model = QueueSearchModel(read: { try await reader.read($0) })
    defer { model.stop() }
    model.activate(in: searchZone()); model.edit("a"); model.search(); model.cancel()
    try await searchEventually { await MainActor.run { !model.loading } }
    #expect(await reader.requests.isEmpty)
    #expect(model.observation == nil)
}

@Test @MainActor func queueSearchFailedContinuationRetainsOnlyStalePreviousCoverage() async throws {
    for changeLength in [false, true] {
        let reader = SearchReader(), model = QueueSearchModel(read: { try await reader.read($0) })
        defer { model.stop() }
        model.activate(in: searchZone()); model.edit("a"); model.search()
        try await searchEventually { await MainActor.run { model.isCurrent && !model.loading } }
        let old = try #require(model.observation)
        if changeLength { await reader.changeTotal() } else { await reader.fail() }
        model.continueSearch()
        try await searchEventually { await MainActor.run { !model.loading } }
        #expect(!model.isCurrent && !model.canContinue && model.problem != nil)
        #expect(model.observation?.id == old.id)
        #expect(throws: ContractError.self) { try model.selection(old.page.items[0], from: old.id) }
        #expect(await reader.requests.count == 2)
    }
}

@Test @MainActor func queueSearchMissingRendererAndForeignOccurrencesCannotBecomeCommands() async throws {
    let model = QueueSearchModel(read: { request in
        searchPage(request, items: [searchItem(1, uri: nil), searchItem(4, uri: "NOT_IMPLEMENTED"), searchItem(9)])
    })
    defer { model.stop() }
    model.activate(in: searchZone()); model.edit("a"); model.search()
    try await searchEventually { await MainActor.run { model.isCurrent } }
    let observed = try #require(model.observation)
    for item in [observed.page.items[0], observed.page.items[1], searchItem(8), searchItem(9, uri: "fixture:wrong")] {
        #expect(throws: ContractError.self) { try model.selection(item, from: observed.id) }
    }
    #expect(throws: ContractError.self) { try model.selection(observed.page.items[2], from: UUID()) }
    #expect(try model.selection(observed.page.items[2], from: observed.id).item.position == 9)
}
