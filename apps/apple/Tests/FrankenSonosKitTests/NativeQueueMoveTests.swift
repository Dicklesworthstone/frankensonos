import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

private func movePage(total: UInt32 = 6, offset: UInt32 = 0, revision: String? = "move:r1",
                      playing: UInt32? = 2, replacements: [UInt32: String] = [:]) -> QueuePage {
    let count = min(25, total > offset ? total - offset : 0)
    let items = (0..<count).map { index in
        let position = offset + index + 1
        let uri = replacements[position] ?? (position == 2 || position == 5 ? "renderer:duplicate" : "renderer:track\(position)")
        return QueueItem(position: position, title: "Track \(position)", uri: uri, creator: "Fixture artist", album: nil)
    }
    return QueuePage(total: total, items: items, playing: playing, revision: revision)
}

private func move(_ position: UInt32 = 5, to destination: QueueMoveDestination = .end) throws -> QueueMove {
    let page = movePage()
    return try QueueMove(#require(page.items.first { $0.position == position }), in: page, to: destination)
}

private func movedJSON(_ move: QueueMove, recovered: Bool = false,
                       mutate: (inout [String: Any]) -> Void = { _ in }) throws -> String {
    var body: [String: Any] = ["done": "Moved the selected entry", "zone": "Studio",
                              "position": move.resultingPosition, "count": 1, "total": move.total,
                              "revision": "move:r2", "changed": true, "recovered": recovered, "undoable": false]
    mutate(&body)
    return String(decoding: try JSONSerialization.data(withJSONObject: body), as: UTF8.self)
}

private actor QueueMoveTransport: DaemonTransport {
    var requests: [URLRequest] = []
    var total: UInt32 = 6
    var revision = "move:r1"
    var playing: UInt32? = 2
    var replacements: [UInt32: String] = [:]
    var blocked = false
    private var holdQueue = false
    private var gate: CheckedContinuation<Void, Never>?

    func setTotal(_ value: UInt32) { total = value }
    func setRevision(_ value: String) { revision = value }
    func setPlaying(_ value: UInt32?) { playing = value }
    func replaceURI(at position: UInt32) { replacements[position] = "renderer:replaced" }
    func blockNextQueue() { holdQueue = true }
    func release() { let saved = gate; gate = nil; blocked = false; saved?.resume() }
    func writes() -> [URLRequest] { requests.filter { $0.httpMethod == "POST" } }

    func send(_ request: URLRequest) async throws -> HTTPReply {
        requests.append(request)
        let url = request.url!, path = url.path.removingPercentEncoding ?? url.path
        if request.httpMethod == "POST" {
            #expect(path == "/queue/move")
            let body = try JSONDecoder().decode([String: JSONValue].self, from: request.httpBody!)
            guard case .integer(let source)? = body["position"], case .integer(let before)? = body["insert_before"] else {
                throw ContractError.invalid("Missing queue move positions in test request")
            }
            return try json(["done": "Moved the selected entry", "zone": "Studio",
                             "position": before < source ? before : before - 1,
                             "count": 1, "total": total, "revision": "move:r2",
                             "changed": true, "recovered": false, "undoable": false])
        }
        if path == "/zones" {
            return HTTPReply(data: Data("[\(zoneJSON),\(zoneJSON.replacingOccurrences(of: "S2", with: "S1"))]".utf8), status: 200)
        }
        if path.hasSuffix("/state") {
            let household = path.contains("@S1/") ? "S1" : "S2"
            return HTTPReply(data: Data(stateJSON.replacingOccurrences(of: "S2", with: household).utf8), status: 200)
        }
        if path.hasSuffix("/queue") {
            if holdQueue {
                holdQueue = false; blocked = true
                // Ignore cancellation to verify the review generation fences it.
                await withCheckedContinuation { gate = $0 }
            }
            let offset = URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems?.first { $0.name == "offset" }?.value
            let page = movePage(total: total, offset: UInt32(offset ?? "") ?? 0, revision: revision,
                                playing: playing, replacements: replacements)
            let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
            return HTTPReply(data: try encoder.encode(page), status: 200)
        }
        throw ContractError.invalid("Unexpected test route: \(path)")
    }

    nonisolated func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> {
        AsyncThrowingStream { $0.finish() }
    }
    private func json(_ object: Any) throws -> HTTPReply {
        HTTPReply(data: try JSONSerialization.data(withJSONObject: object), status: 200)
    }
}

private func moveClient(_ transport: QueueMoveTransport) throws -> DaemonClient {
    DaemonClient(endpoint: try DaemonEndpoint("https://music.example.test"), transport: transport)
}

@Suite("Native queue arrangement") struct NativeQueueMoveTests {
    @Test func shortcutsUseOriginalCoordinatesAndPreserveDuplicateOccurrences() throws {
        let cases: [(UInt32, QueueMoveDestination, UInt32, UInt32)] = [
            (3, .earlier, 2, 2), (3, .later, 5, 4), (3, .start, 1, 1), (3, .end, 7, 6),
            (5, .afterCurrent, 3, 3), (1, .afterCurrent, 3, 2),
            (2, .earlier, 1, 1), (2, .later, 4, 3), (2, .start, 1, 1), (2, .end, 7, 6)
        ]
        for (position, destination, before, result) in cases {
            let chosen = try move(position, to: destination)
            #expect(chosen.insertBefore == before); #expect(chosen.resultingPosition == result)
            let body = try NativeCommand.queueMove(chosen).payload(in: testZone())
            #expect(body.path == ["queue", "move"])
            #expect(body.body == ["zone": .string("Studio@S2"), "position": .integer(Int64(position)),
                                 "insert_before": .integer(Int64(before)), "expected_revision": .string("move:r1"),
                                 "expected_uris": .array([.string(chosen.item.uri!)])])
        }
        let firstCopy = try move(2), secondCopy = try move(5)
        #expect(firstCopy.item.uri == secondCopy.item.uri)
        #expect(firstCopy.item.position != secondCopy.item.position)
        #expect(secondCopy.explanation.contains("Track 5"))
        #expect(secondCopy.explanation.contains("position 5"))
        #expect(secondCopy.explanation.contains("position 6 of 6"))
        #expect(secondCopy.explanation.contains("cannot be undone"))
    }

    @Test func boundaryAndAlreadyNextActionsAreUnavailable() throws {
        for (position, destination) in [(UInt32(1), QueueMoveDestination.earlier), (1, .start),
                                        (6, .later), (6, .end), (2, .afterCurrent), (3, .afterCurrent)] {
            #expect(throws: ContractError.self) { try move(position, to: destination) }
        }
        let unknown = movePage(playing: nil)
        #expect(throws: ContractError.self) { try QueueMove(unknown.items[4], in: unknown, to: .afterCurrent) }
        // Moving the currently playing copy is supported for absolute choices.
        for destination in [QueueMoveDestination.earlier, .later, .start, .end] {
            #expect(try move(2, to: destination).item.position == 2)
        }
    }

    @Test func missingIdentityAndInconsistentObservationsCannotOfferMoves() throws {
        let page = movePage(), item = page.items[2]
        for uri in [nil, "", " \n ", "NOT_IMPLEMENTED"] as [String?] {
            let invalid = QueueItem(position: 3, title: item.title, uri: uri, creator: nil, album: nil)
            let badPage = QueuePage(total: 6, items: [invalid], playing: 2, revision: "move:r1")
            #expect(throws: ContractError.self) { try QueueMove(invalid, in: badPage, to: .end) }
        }
        for revision in [nil, "", " "] as [String?] {
            let badPage = movePage(revision: revision)
            #expect(throws: ContractError.self) { try QueueMove(item, in: badPage, to: .end) }
        }
        for playing in [0, 7] as [UInt32?] {
            let badPage = movePage(playing: playing)
            #expect(throws: ContractError.self) { try QueueMove(item, in: badPage, to: .end) }
        }
        let other = QueueItem(position: 3, title: item.title, uri: "renderer:other", creator: nil, album: nil)
        #expect(throws: ContractError.self) { try QueueMove(other, in: page, to: .earlier) }
        let gap = QueuePage(total: 6, items: [page.items[0], page.items[2]], playing: 2, revision: "move:r1")
        #expect(throws: ContractError.self) { try QueueMove(item, in: gap, to: .earlier) }
    }

    @Test func hugeQueueBoundariesDoNotOverflowOrClamp() throws {
        let item = QueueItem(position: UInt32.max - 1, title: "Near end", uri: "renderer:nearEnd", creator: nil, album: nil)
        let page = QueuePage(total: .max, items: [item], playing: .max, revision: "move:r1")
        for destination in [QueueMoveDestination.later, .end, .afterCurrent] {
            #expect(throws: ContractError.self) { try QueueMove(item, in: page, to: destination) }
        }
        #expect(try QueueMove(item, in: page, to: .earlier).insertBefore == UInt32.max - 2)
        #expect(try QueueMove(item, in: page, to: .start).resultingPosition == 1)
    }

    @Test(arguments: ["S1", "S2"])
    func reviewedMoveWritesOnceToTheCapturedHousehold(household: String) async throws {
        let transport = QueueMoveTransport(), chosen = try move(5)
        let session = try CommandReviewSession(client: moveClient(transport))
        let zone = try fixture(Zone.self, zoneJSON.replacingOccurrences(of: "S2", with: household))
        let review = try await session.prepare(.queueMove(chosen), in: zone)
        #expect(await transport.writes().isEmpty)
        let receipt = try await session.execute(review)
        #expect(!receipt.partial)
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        let writes = await transport.writes()
        #expect(writes.count == 1)
        let body = try JSONDecoder().decode([String: JSONValue].self, from: #require(writes.first?.httpBody))
        #expect(body["zone"] == .string("Studio@\(household)"))
        #expect(body["position"] == .integer(5)); #expect(body["insert_before"] == .integer(7))
        #expect(body["expected_uris"] == .array([.string("renderer:duplicate")]))
        let queues = await transport.requests.filter { $0.url?.path.hasSuffix("/queue") == true }
        #expect(queues.count == 2)
        #expect(queues.allSatisfy { URLComponents(url: $0.url!, resolvingAgainstBaseURL: false)?.queryItems?.contains(URLQueryItem(name: "offset", value: "4")) == true })
        #expect(writes.allSatisfy { $0.url?.path == "/queue/move" })
    }

    @Test(arguments: [false, true])
    func staleSourceRevisionAndTotalFailBeforeAnyWrite(afterReview: Bool) async throws {
        for change in 0..<3 {
            let transport = QueueMoveTransport(), session = try CommandReviewSession(client: moveClient(transport))
            let command = NativeCommand.queueMove(try move(3))
            let review = afterReview ? try await session.prepare(command, in: testZone()) : nil
            switch change {
            case 0: await transport.setRevision("move:new")
            case 1: await transport.setTotal(7)
            default: await transport.replaceURI(at: 3)
            }
            if let review { await #expect(throws: ContractError.self) { try await session.execute(review) } }
            else { await #expect(throws: ContractError.self) { try await session.prepare(command, in: testZone()) } }
            #expect(await transport.writes().isEmpty)
        }
    }

    @Test(arguments: [false, true])
    func afterCurrentCannotFollowADifferentAnchorWithTheSameRevision(afterReview: Bool) async throws {
        for current in [nil, 3, 0, 7] as [UInt32?] {
            let transport = QueueMoveTransport(), session = try CommandReviewSession(client: moveClient(transport))
            let command = NativeCommand.queueMove(try move(5, to: .afterCurrent))
            let review = afterReview ? try await session.prepare(command, in: testZone()) : nil
            await transport.setPlaying(current)
            if let review { await #expect(throws: ContractError.self) { try await session.execute(review) } }
            else { await #expect(throws: ContractError.self) { try await session.prepare(command, in: testZone()) } }
            #expect(await transport.writes().isEmpty)
        }
    }

    @Test func movingCurrentDuplicateDoesNotRestartPlaybackOrRetargetAnotherCopy() async throws {
        let transport = QueueMoveTransport(), session = try CommandReviewSession(client: moveClient(transport))
        let review = try await session.prepare(.queueMove(move(2, to: .end)), in: testZone())
        // An absolute queue move does not acquire the currently playing slot.
        await transport.setPlaying(4)
        _ = try await session.execute(review)
        let sent = try #require(await transport.writes().first)
        #expect(sent.url?.path == "/queue/move")
        let body = try JSONDecoder().decode([String: JSONValue].self, from: #require(sent.httpBody))
        #expect(body["position"] == .integer(2)); #expect(body["expected_uris"] == .array([.string("renderer:duplicate")]))
        #expect(await transport.writes().count == 1)
    }

    @Test func cancellationDuringFreshQueueReadCannotSendTheMoveLater() async throws {
        let transport = QueueMoveTransport(), session = try CommandReviewSession(client: moveClient(transport))
        let review = try await session.prepare(.queueMove(move()), in: testZone())
        await transport.blockNextQueue()
        let pending = Task { try await session.execute(review) }
        let deadline = ContinuousClock.now.advanced(by: .seconds(2))
        while !(await transport.blocked) {
            guard ContinuousClock.now < deadline else { throw URLError(.timedOut) }
            try await Task.sleep(for: .milliseconds(2))
        }
        await session.cancel(); await transport.release()
        await #expect(throws: CancellationError.self) { try await pending.value }
        #expect(await transport.writes().isEmpty)
    }

    @Test(arguments: [false, true])
    func acknowledgedAndRecoveredMovesAreConfirmedReceipts(recovered: Bool) async throws {
        let chosen = try move(), transport = try RecordingTransport(movedJSON(chosen, recovered: recovered))
        let result = try await client(transport).perform(.queueMove(chosen), in: testZone())
        #expect(!result.partial); #expect(result.detail.contains("position 5 to 6 of 6"))
        #expect(result.detail.contains("confirmed by reading") == recovered)
        #expect(await transport.requests.count == 1)
    }

    @Test func missingMismatchedAndLostReceiptsAreUncertainAndNeverRetried() async throws {
        let chosen = try move()
        let changes: [[String: Any]] = [["zone": "Hall"], ["position": 5], ["count": 2], ["total": 7],
            ["revision": "move:r1"], ["revision": " "], ["changed": false], ["undoable": true],
            ["recovered": "true"], ["done": ""]]
        var transports = try changes.map { change in RecordingTransport(try movedJSON(chosen) { $0.merge(change) { _, new in new } }) }
        for missing in ["position", "total", "revision", "recovered", "changed", "undoable"] {
            transports.append(try RecordingTransport(movedJSON(chosen) { $0.removeValue(forKey: missing) }))
        }
        transports += [RecordingTransport("not JSON"), RecordingTransport(failure: true)]
        for transport in transports {
            do {
                _ = try await client(transport).perform(.queueMove(chosen), in: testZone())
                Issue.record("Expected an uncertain queue move")
            } catch let error as ClientError {
                guard case .uncertain = error else { Issue.record("Expected uncertainty, got \(error)"); continue }
            }
            #expect(await transport.requests.count == 1)
        }
    }
}
