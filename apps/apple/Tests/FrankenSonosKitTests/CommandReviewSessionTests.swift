import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

// Synthetic wire replies, not site data. Exercise the production HTTP client,
// codecs, bounded room reader and command session through an injected transport.
actor ReviewTransport: DaemonTransport {
    var requests: [URLRequest] = []
    var volumes = ["Studio@TEST-A": 20, "Hall@TEST-A": 40]
    var uri = "test:track:one", position = 12
    var revision = "queue-r1", favoriteTitle = "Test station", workTitle = "Test work"
    var unavailable: String?
    var destinationExpanded = false
    var lostWrite = false, malformedWrite = false, wrongFeedback = false
    private var gatePath: String?
    private var gate: CheckedContinuation<Void, Never>?
    var blocked = false

    func setVolume(_ room: String, _ value: Int) { volumes[room] = value }
    func setURI(_ value: String) { uri = value }
    func setPosition(_ value: Int) { position = value }
    func setRevision(_ value: String) { revision = value }
    func setFavorite(_ value: String) { favoriteTitle = value }
    func setWork(_ value: String) { workTitle = value }
    func setUnavailable(_ value: String?) { unavailable = value }
    func expandDestination() { destinationExpanded = true }
    func loseWrite() { lostWrite = true }
    func breakWrite() { malformedWrite = true }
    func mismatchFeedback() { wrongFeedback = true }
    func blockNext(_ path: String) { gatePath = path; blocked = false }
    func release() { let saved = gate; gate = nil; blocked = false; saved?.resume() }
    func writes() -> [URLRequest] { requests.filter { $0.httpMethod == "POST" } }

    func zones() -> [Zone] {
        [reviewZone(), reviewZone(house: "TEST-B"),
         Zone(household: "TEST-A", coordinatorRoom: "Deck", members: destinationExpanded ? ["Deck", "Porch"] : ["Deck"], transportState: "playing")]
    }
    func send(_ request: URLRequest) async throws -> HTTPReply {
        requests.append(request)
        let url = request.url!, path = url.path.removingPercentEncoding ?? url.path
        if gatePath == path {
            gatePath = nil; blocked = true
            // Deliberately ignore task cancellation until released.
            await withCheckedContinuation { gate = $0 }
        }
        if request.httpMethod == "POST" {
            if lostWrite { throw URLError(.networkConnectionLost) }
            if malformedWrite { return HTTPReply(data: Data("incomplete".utf8), status: 200) }
            let body = try JSONDecoder().decode([String: JSONValue].self, from: request.httpBody!)
            if path == "/dj/feedback" {
                let signal = wrongFeedback ? "dislike" : "like"
                return try json(["zone": "Studio", "work": workTitle, "composer": "Test artist", "signal": signal, "done": "Feedback recorded"])
            }
            if path == "/queue/remove" {
                return try json(["done": "Removed", "position": 1, "count": 1, "remaining": 0, "undoable": false])
            }
            if path == "/queue" { return try json(["done": "Queued", "first": 2, "count": 1, "playing_queue": false]) }
            if path == "/volume" {
                return try json(["done": "Set level", "changed": true, "volume": 25,
                                 "notes": [["code": "VOLUME_CLAMPED", "detail": "Capped at 25"]]])
            }
            #expect(body["zone"] != nil)
            return try json(["done": "Applied", "changed": true])
        }
        if path == "/zones" {
            let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
            return HTTPReply(data: try encoder.encode(zones()), status: 200)
        }
        if path.hasSuffix("/state") {
            let room = path.split(separator: "/")[1].description
            if unavailable == room { throw URLError(.cannotConnectToHost) }
            let zone = zones().first { $0.rooms.contains { $0.address == room } }!
            let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
            let object = try JSONSerialization.jsonObject(with: encoder.encode(zone))
            return try json(["zone": object, "transport_state": "playing", "volume": volumes[room] ?? 20,
                             "group_volume": ((volumes["Studio@TEST-A"] ?? 20) + (volumes["Hall@TEST-A"] ?? 40)) / 2,
                             "mute": false, "track": ["uri": uri, "duration_secs": 240, "position_secs": position, "queue_position": 1]])
        }
        if path.hasSuffix("/queue") {
            return try json(["total": 1, "revision": revision, "items": [["position": 1, "title": "Test entry", "uri": "test:entry"]]])
        }
        if path == "/favorites" {
            return try json([["id": "FV:1", "title": favoriteTitle, "kind": "stream"]])
        }
        if path.hasSuffix("/dj") {
            return try json(["zone": "Studio", "running": true,
                             "now": ["title": workTitle, "composer": "Test artist", "movements": 1, "reason": ["summary": "Variety"]],
                             "next": [], "steering": ["source": "program"]])
        }
        throw ContractError.invalid("Unexpected test route: \(path)")
    }
    nonisolated func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> {
        AsyncThrowingStream { $0.finish() }
    }
    private func json(_ object: Any) throws -> HTTPReply {
        HTTPReply(data: try JSONSerialization.data(withJSONObject: object, options: [.sortedKeys]), status: 200)
    }
}

func reviewZone(house: String = "TEST-A") -> Zone {
    Zone(household: house, coordinatorRoom: "Studio", members: ["Studio", "Hall"], transportState: "playing")
}
func reviewClient(_ transport: ReviewTransport) throws -> DaemonClient {
    DaemonClient(endpoint: try DaemonEndpoint("https://music.example.test"), transport: transport)
}
func waitForReviewGate(_ transport: ReviewTransport) async throws {
    let deadline = ContinuousClock.now.advanced(by: .seconds(2))
    while !(await transport.blocked) {
        guard ContinuousClock.now < deadline else { throw ContractError.invalid("Test gate was not reached") }
        try await Task.sleep(for: .milliseconds(2))
    }
}
func reviewItem() -> QueueItem { QueueItem(position: 1, title: "Test entry", uri: "test:entry", creator: nil, album: nil) }

@Test func reviewedActionReadsTwiceThenSendsExactlyOnce() async throws {
    let transport = ReviewTransport(), zone = reviewZone()
    let session = try CommandReviewSession(client: reviewClient(transport))
    let review = try await session.prepare(.pause, in: zone)
    #expect(await transport.writes().isEmpty)
    #expect(review.state.volume == 20)
    let receipt = try await session.execute(review)
    #expect(receipt.title == "Applied")
    #expect(await transport.writes().count == 1)
    await #expect(throws: ContractError.self) { try await session.execute(review) }
    #expect(await transport.writes().count == 1)
    #expect(await transport.requests.filter { $0.url?.path == "/zones" }.count == 2)
}

@Test func reviewedMemberVolumeKeepsHouseholdQualifiedIdentity() async throws {
    let transport = ReviewTransport()
    let session = try CommandReviewSession(client: reviewClient(transport))
    let review = try await session.prepare(.volume(80, room: "Hall"), in: reviewZone(house: "TEST-B"))
    let receipt = try await session.execute(review)
    let request = try #require(await transport.writes().first)
    let body = try JSONDecoder().decode([String: JSONValue].self, from: request.httpBody!)
    #expect(body["zone"] == .string("Hall@TEST-B"))
    #expect(receipt.detail.contains("Capped at 25"))
}

@Test func unrelatedOfflineRoomDoesNotPreventCoordinatorControl() async throws {
    let transport = ReviewTransport()
    await transport.setUnavailable("Hall@TEST-A")
    let session = try CommandReviewSession(client: reviewClient(transport))
    _ = try await session.execute(session.prepare(.pause, in: reviewZone()))
    #expect(await transport.writes().count == 1)
}

@Test func failedAffectedReadNeverDispatchesOrPublishesAReview() async throws {
    let transport = ReviewTransport()
    await transport.setUnavailable("Hall@TEST-A")
    let session = try CommandReviewSession(client: reviewClient(transport))
    await #expect(throws: ContractError.self) { try await session.prepare(.groupVolume(25), in: reviewZone()) }
    #expect(await transport.writes().isEmpty)
}

@Test func unchangedAverageCannotHideChangedMemberLevels() async throws {
    let transport = ReviewTransport()
    let session = try CommandReviewSession(client: reviewClient(transport))
    let review = try await session.prepare(.groupVolume(25), in: reviewZone())
    await transport.setVolume("Studio@TEST-A", 30)
    await transport.setVolume("Hall@TEST-A", 30)
    await #expect(throws: ContractError.self) { try await session.execute(review) }
    #expect(await transport.writes().isEmpty)
}

@Test func playbackChangeRefusesTransportCommandButClockProgressAllowsSeek() async throws {
    let transport = ReviewTransport()
    let session = try CommandReviewSession(client: reviewClient(transport))
    let review = try await session.prepare(.next, in: reviewZone())
    await transport.setURI("test:another-track")
    await #expect(throws: ContractError.self) { try await session.execute(review) }
    let seek = try await session.prepare(.seek(60), in: reviewZone())
    await transport.setPosition(30)
    _ = try await session.execute(seek)
    #expect(await transport.writes().map { $0.url?.path } == ["/seek"])
}

@Test func appendingDoesNotNeedUnchangedPlaybackButQueueNextDoes() async throws {
    let transport = ReviewTransport()
    let session = try CommandReviewSession(client: reviewClient(transport))
    let append = try await session.prepare(.queueAdd("test:album", next: false), in: reviewZone())
    await transport.setURI("test:another-track")
    _ = try await session.execute(append)
    let next = try await session.prepare(.queueAdd("test:album", next: true), in: reviewZone())
    await transport.setURI("test:yet-another-track")
    await #expect(throws: ContractError.self) { try await session.execute(next) }
    #expect(await transport.writes().count == 1)
}

@Test func queueReviewBindsTheObservedRevisionAndOccurrence() async throws {
    let transport = ReviewTransport()
    let session = try CommandReviewSession(client: reviewClient(transport))
    let review = try await session.prepare(.queuePlay(reviewItem(), revision: "queue-r1"), in: reviewZone())
    await transport.setRevision("queue-r2")
    await #expect(throws: ContractError.self) { try await session.execute(review) }
    #expect(await transport.writes().isEmpty)
    let remove = try await session.prepare(.queueRemove(reviewItem(), revision: "queue-r2"), in: reviewZone())
    #expect(try await session.execute(remove).detail.contains("cannot be undone"))
    let request = try #require(await transport.writes().first)
    let body = try JSONDecoder().decode([String: JSONValue].self, from: request.httpBody!)
    #expect(body["expected_revision"] == .string("queue-r2"))
    #expect(body["expected_uris"] == .array([.string("test:entry")]))
}

@Test func favoriteReviewDetectsReplacementAndShowsCurrentLabel() async throws {
    let transport = ReviewTransport()
    let session = try CommandReviewSession(client: reviewClient(transport))
    let review = try await session.prepare(.playFavorite("FV:1"), in: reviewZone())
    #expect(review.favorite?.title == "Test station")
    await transport.setFavorite("Replacement station")
    await #expect(throws: ContractError.self) { try await session.execute(review) }
    let refreshed = try await session.prepare(.playFavorite("FV:1"), in: reviewZone())
    #expect(refreshed.favorite?.title == "Replacement station")
    _ = try await session.execute(refreshed)
    #expect(await transport.writes().count == 1)
}

@Test func feedbackAcceptsTheRealBackendDTOWithoutChanged() async throws {
    let transport = ReviewTransport()
    let session = try CommandReviewSession(client: reviewClient(transport))
    let review = try await session.prepare(.djFeedback(like: true), in: reviewZone())
    let receipt = try await session.execute(review)
    #expect(receipt.title == "Feedback recorded")
    #expect(receipt.detail.contains("Test work"))
    #expect(await transport.writes().count == 1)
}

@Test func djReviewDoesNotSendFeedbackForAChangedSelection() async throws {
    let transport = ReviewTransport()
    let session = try CommandReviewSession(client: reviewClient(transport))
    let review = try await session.prepare(.djFeedback(like: true), in: reviewZone())
    await transport.setWork("Different selection")
    await #expect(throws: ContractError.self) { try await session.execute(review) }
    #expect(await transport.writes().isEmpty)
}

@Test func groupingReviewIncludesDestinationMembership() async throws {
    let transport = ReviewTransport()
    let session = try CommandReviewSession(client: reviewClient(transport))
    let command = NativeCommand.join(room: "Hall", to: RoomID(household: "TEST-A", room: "Deck"))
    let review = try await session.prepare(command, in: reviewZone())
    await transport.expandDestination()
    await #expect(throws: ContractError.self) { try await session.execute(review) }
    #expect(await transport.writes().isEmpty)
    _ = try await session.execute(session.prepare(command, in: reviewZone()))
    #expect(await transport.writes().count == 1)
}

@Test func supersededAndForeignReviewsCannotExecute() async throws {
    let transport = ReviewTransport()
    let client = try reviewClient(transport)
    let session = CommandReviewSession(client: client), other = CommandReviewSession(client: client)
    let old = try await session.prepare(.pause, in: reviewZone())
    let current = try await session.prepare(.next, in: reviewZone())
    await #expect(throws: ContractError.self) { try await session.execute(old) }
    await #expect(throws: ContractError.self) { try await other.execute(current) }
    _ = try await session.execute(current)
    #expect(await transport.writes().count == 1)
}

@Test func cancellingPendingAndInFlightReviewsNeverSends() async throws {
    let transport = ReviewTransport()
    let session = try CommandReviewSession(client: reviewClient(transport))
    let review = try await session.prepare(.pause, in: reviewZone())
    await session.cancel()
    await #expect(throws: ContractError.self) { try await session.execute(review) }
    await transport.blockNext("/zones")
    let task = Task { try await session.prepare(.pause, in: reviewZone()) }
    try await waitForReviewGate(transport)
    await session.cancel()
    await transport.release()
    await #expect(throws: CancellationError.self) { try await task.value }
    #expect(await transport.writes().isEmpty)
}

@Test func cancellingConfirmationAfterItsReadPreventsDispatch() async throws {
    let transport = ReviewTransport()
    let session = try CommandReviewSession(client: reviewClient(transport))
    let review = try await session.prepare(.pause, in: reviewZone())
    await transport.blockNext("/zones")
    let task = Task { try await session.execute(review) }
    try await waitForReviewGate(transport)
    await session.cancel()
    await transport.release()
    await #expect(throws: CancellationError.self) { try await task.value }
    #expect(await transport.writes().isEmpty)
}

@Test func clientCancellationDuringItsFinalTopologyReadIsDefinite() async throws {
    let transport = ReviewTransport()
    let client = try reviewClient(transport)
    await transport.blockNext("/zones")
    let task = Task { try await client.perform(.party, in: reviewZone()) }
    try await waitForReviewGate(transport)
    task.cancel()
    await transport.release()
    await #expect(throws: CancellationError.self) { try await task.value }
    #expect(await transport.writes().isEmpty)
}

@Test func slowWriteSerializesCommandsAndKeepsItsReceipt() async throws {
    let transport = ReviewTransport()
    let session = try CommandReviewSession(client: reviewClient(transport))
    let review = try await session.prepare(.pause, in: reviewZone())
    await transport.blockNext("/pause")
    let task = Task { try await session.execute(review) }
    try await waitForReviewGate(transport)
    await #expect(throws: ContractError.self) { try await session.execute(review) }
    await #expect(throws: ContractError.self) { try await session.prepare(.next, in: reviewZone()) }
    await session.cancel()
    await transport.release()
    #expect(try await task.value.title == "Applied")
    #expect(await transport.writes().count == 1)
}

@Test func uncertainAndMalformedWriteRepliesConsumeReviewWithoutReplay() async throws {
    for malformed in [false, true] {
        let transport = ReviewTransport()
        if malformed { await transport.breakWrite() } else { await transport.loseWrite() }
        let session = try CommandReviewSession(client: reviewClient(transport))
        let review = try await session.prepare(.next, in: reviewZone())
        await #expect(throws: ClientError.self) { try await session.execute(review) }
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes().count == 1)
    }
}

@Test func contradictoryFeedbackReceiptRemainsUncertainAndIsNotRepeated() async throws {
    let transport = ReviewTransport()
    await transport.mismatchFeedback()
    let session = try CommandReviewSession(client: reviewClient(transport))
    let review = try await session.prepare(.djFeedback(like: true), in: reviewZone())
    await #expect(throws: ClientError.self) { try await session.execute(review) }
    #expect(await transport.writes().count == 1)
}
