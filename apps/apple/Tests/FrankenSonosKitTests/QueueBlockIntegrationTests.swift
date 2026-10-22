import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

private func integratedBlockPage(_ offset: UInt32, size: UInt32 = 25, replaced: Bool = false) -> QueuePage {
    let items = (0..<min(size, 120 - offset)).map { index in
        let position = offset + index + 1
        return QueueItem(position: position, title: "Movement \(position)",
                         uri: replaced && position == 64 ? "renderer:replaced" : "renderer:movement\(position % 9)",
                         creator: "Fixture composer", album: "Fixture work")
    }
    return QueuePage(total: 120, items: items, playing: 2, revision: "block:original")
}

private actor BlockCommandTransport: DaemonTransport {
    var requests: [URLRequest] = []
    var queueOffsets: [UInt32] = []
    var replaced = false, loseReply = false, shortReceipt = false, blocked = false
    private var heldOffset: UInt32?
    private var gate: CheckedContinuation<Void, Never>?
    func replaceLaterEntry() { replaced = true }
    func loseNextReply(short: Bool) { loseReply = !short; shortReceipt = short }
    func hold(_ offset: UInt32) { heldOffset = offset }
    func release() { gate?.resume(); gate = nil; blocked = false }
    func writes() -> [URLRequest] { requests.filter { $0.httpMethod == "POST" } }
    func send(_ request: URLRequest) async throws -> HTTPReply {
        requests.append(request)
        let path = request.url!.path.removingPercentEncoding ?? request.url!.path
        if request.httpMethod == "POST" {
            #expect(path == "/queue/move")
            if loseReply { throw URLError(.networkConnectionLost) }
            return HTTPReply(data: Data("""
                {"done":"Moved block","zone":"Studio","position":1,"count":\(shortReceipt ? 1 : 60),
                 "total":120,"revision":"block:after","changed":true,"recovered":false,"undoable":false}
                """.utf8), status: 200)
        }
        if path == "/zones" {
            return HTTPReply(data: Data("[\(zoneJSON),\(zoneJSON.replacingOccurrences(of: "S2", with: "S1"))]".utf8), status: 200)
        }
        if path.hasSuffix("/state") {
            return HTTPReply(data: Data(stateJSON.replacingOccurrences(of: "S2", with: path.contains("@S1/") ? "S1" : "S2").utf8), status: 200)
        }
        if path.hasSuffix("/queue") {
            let query = URLComponents(url: request.url!, resolvingAgainstBaseURL: false)!.queryItems ?? []
            let offset = UInt32(query.first { $0.name == "offset" }?.value ?? "")!
            #expect(query.contains(URLQueryItem(name: "limit", value: "25")))
            queueOffsets.append(offset)
            if heldOffset == offset {
                heldOffset = nil; blocked = true
                await withCheckedContinuation { gate = $0 }
            }
            let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
            return HTTPReply(data: try encoder.encode(integratedBlockPage(offset, replaced: replaced)), status: 200)
        }
        throw ContractError.invalid("Unexpected block test route")
    }
    nonisolated func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> {
        AsyncThrowingStream { $0.finish() }
    }
}

private func blockSession(_ transport: BlockCommandTransport) throws -> CommandReviewSession {
    CommandReviewSession(client: DaemonClient(endpoint: try DaemonEndpoint("https://music.example.test"), transport: transport))
}
private func integratedMove() throws -> QueueMove {
    let page = integratedBlockPage(25, size: 60)
    return try QueueMove(page.items, in: page, to: .start)
}

@Suite("Native queue block shared-writer integration") struct QueueBlockIntegrationTests {
    @Test(arguments: ["S1", "S2"])
    func readsEveryOccurrenceTwiceThenSendsExactlyOneMove(home: String) async throws {
        let transport = BlockCommandTransport(), session = try blockSession(transport)
        let zone = try fixture(Zone.self, zoneJSON.replacingOccurrences(of: "S2", with: home))
        let move = try integratedMove()
        let review = try await session.prepare(.queueMove(move), in: zone)
        #expect(await transport.queueOffsets == [25, 50, 75])
        #expect(await transport.writes().isEmpty)
        let receipt = try await session.execute(review)
        #expect(!receipt.partial && receipt.detail.contains("60 entries"))
        #expect(await transport.queueOffsets == [25, 50, 75, 25, 50, 75])
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        let writes = await transport.writes()
        #expect(writes.count == 1)
        let bodyData = try #require(writes.first?.httpBody)
        let body = try JSONDecoder().decode([String: JSONValue].self, from: bodyData)
        #expect(body == move.fields(in: zone))
        #expect(body["expected_uris"] == .array(move.items.map { .string($0.uri!) }))
        #expect(writes.first?.url?.path == "/queue/move")
    }

    @Test func aChangedOccurrenceOnTheSecondPageStopsTheEntireMove() async throws {
        let transport = BlockCommandTransport(), session = try blockSession(transport)
        let review = try await session.prepare(.queueMove(integratedMove()), in: testZone())
        await transport.replaceLaterEntry()
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.queueOffsets == [25, 50, 75, 25, 50])
        #expect(await transport.writes().isEmpty)
    }

    @Test func cancellationOnALaterPageKeepsTheWriteGateUntilDrain() async throws {
        let transport = BlockCommandTransport(), session = try blockSession(transport)
        let review = try await session.prepare(.queueMove(integratedMove()), in: testZone())
        await transport.hold(50)
        let task = Task { try await session.execute(review) }
        let deadline = ContinuousClock.now.advanced(by: .seconds(3))
        while !(await transport.blocked) {
            guard ContinuousClock.now < deadline else { task.cancel(); throw URLError(.timedOut) }
            try await Task.sleep(for: .milliseconds(2))
        }
        task.cancel(); await session.cancel()
        await #expect(throws: ContractError.self) { try await session.prepare(.pause, in: testZone()) }
        await transport.release()
        await #expect(throws: CancellationError.self) { try await task.value }
        #expect(await transport.queueOffsets == [25, 50, 75, 25, 50])
        #expect(await transport.writes().isEmpty)
    }

    @Test(arguments: [false, true])
    func lostOrIncompleteAcknowledgementIsUncertainAndNeverRepeated(short: Bool) async throws {
        let transport = BlockCommandTransport(), session = try blockSession(transport)
        let review = try await session.prepare(.queueMove(integratedMove()), in: testZone())
        await transport.loseNextReply(short: short)
        do {
            _ = try await session.execute(review)
            Issue.record("An incomplete block move must not be acknowledged")
        } catch ClientError.uncertain { }
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes().count == 1)
    }
}
