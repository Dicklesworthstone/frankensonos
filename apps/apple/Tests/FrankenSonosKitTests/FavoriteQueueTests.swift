import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

private func favoriteQueueZone(_ home: String = "S2", members: [String] = ["Room", "Member"]) -> Zone {
    Zone(household: home, coordinatorRoom: members[0], members: members, transportState: "playing")
}
private func queueFavorite(_ id: String = "FV:2/7", title: String = "Dvořák & friends", kind: String = "container",
                           description: String? = "Saved album") -> Favorite {
    Favorite(id: id, title: title, kind: kind, description: description)
}
private func favoriteQueueState(_ zone: Zone, uri: String = "fixture:one", at: UInt32 = 10) -> ZoneState {
    ZoneState(zone: zone, transportState: "playing", volume: 20, mute: false, groupVolume: 20,
        track: Track(uri: uri, title: "Playing", creator: nil, album: nil, durationSecs: 300,
                     positionSecs: at, queuePosition: 1), shuffle: false, repeat: "off")
}
private func favoriteQueueReply(_ changes: [String: JSONValue] = [:]) throws -> Data {
    var body: [String: JSONValue] = ["zone": .string("Room"), "done": .string("Favorite queued"),
        "first": .integer(2), "count": .integer(3), "playing_queue": .bool(false)]
    body.merge(changes) { _, new in new }
    return try JSONEncoder().encode(body)
}

@Suite("Native household favorite queue") struct FavoriteQueueTests {
    @Test(arguments: ["S1", "S2"], [false, true])
    func exactNativePayloadNeverCarriesURIOrRendererMetadata(home: String, next: Bool) throws {
        let zone = favoriteQueueZone(home), favorite = queueFavorite()
        let command = NativeCommand.queueFavorite(try FavoriteQueue(favorite, in: zone, next: next))
        let payload = try command.payload(in: zone)
        #expect(payload.path == ["queue"])
        #expect(payload.body == ["zone": .string("Room@\(home)"), "favorite": .string(favorite.id), "next": .bool(next)])
        #expect(command.isQueueAdd && !command.requiresDJ && !command.requiresFreshTopology)
        #expect(payload.timeout == 600)
    }

    @Test func onlyTracksAndContainersCanBeQueued() throws {
        for kind in ["track", "container"] {
            #expect(FavoriteQueue.canQueue(queueFavorite(kind: kind)))
        }
        for kind in ["stream", "shortcut", "unknown", "", "Track"] {
            let favorite = queueFavorite(kind: kind)
            #expect(!FavoriteQueue.canQueue(favorite))
            #expect(throws: ContractError.self) { try FavoriteQueue(favorite, in: favoriteQueueZone(), next: false) }
        }
        #expect(queueFavorite(kind: "stream").playable)
    }

    @Test func unreadableIdentitiesCannotBeTrimmedOrReinterpreted() throws {
        for id in ["", " FV:1", "FV:1 ", "FV:\n1", "FV:\0", String(repeating: "x", count: 1_025)] {
            #expect(!FavoriteQueue.canQueue(queueFavorite(id)))
        }
        let boundary = String(repeating: "🎵", count: 1_024)
        #expect(FavoriteQueue.canQueue(queueFavorite(boundary)))
        let value = try FavoriteQueue(queueFavorite(boundary), in: favoriteQueueZone(), next: false)
        #expect(try value.fields(in: favoriteQueueZone())["favorite"] == .string(boundary))
        #expect(!FavoriteQueue.canQueue(queueFavorite(title: " \n")))
    }

    @Test func sameGroupProgressIsAllowedButNeverAnotherHouseholdOrCoordinator() throws {
        let zone = favoriteQueueZone(), value = try FavoriteQueue(queueFavorite(), in: zone, next: true)
        let reordered = Zone(household: "S2", coordinatorRoom: "Room", members: ["Member", "Room"], transportState: "paused")
        #expect(try value.fields(in: reordered)["zone"] == .string("Room@S2"))
        for changed in [favoriteQueueZone("S1"), favoriteQueueZone(members: ["Room"]),
                        favoriteQueueZone(members: ["Member", "Room"])] {
            #expect(throws: ContractError.self) { try value.fields(in: changed) }
        }
    }

    @Test func favoritePreflightRequiresExactIDAndVisibleDefinition() throws {
        let favorite = queueFavorite(), zone = favoriteQueueZone()
        let value = try FavoriteQueue(favorite, in: zone, next: false)
        #expect(try value.validate([queueFavorite("FV:99"), favorite], in: zone) == favorite)
        for list in [[], [queueFavorite("FV:99")], [favorite, favorite],
                     [queueFavorite(title: "Another album")], [queueFavorite(kind: "track")],
                     [queueFavorite(description: nil)], [queueFavorite(kind: "stream")]] {
            #expect(throws: ContractError.self) { try value.validate(list, in: zone) }
        }
    }

    @Test func unicodeNormalizationDoesNotSubstituteAnotherFavorite() throws {
        let zone = favoriteQueueZone()
        let original = queueFavorite("FV:é", title: "é")
        let value = try FavoriteQueue(original, in: zone, next: false)
        #expect(throws: ContractError.self) { try value.validate([queueFavorite("FV:e\u{301}", title: "é")], in: zone) }
        #expect(throws: ContractError.self) { try value.validate([queueFavorite("FV:é", title: "e\u{301}")], in: zone) }
    }

    @Test func appendAllowsTrackAdvanceWhileQueueNextProtectsTheReviewedTrack() throws {
        let zone = favoriteQueueZone(), before = favoriteQueueState(zone)
        let append = NativeCommand.queueFavorite(try FavoriteQueue(queueFavorite(), in: zone, next: false))
        let next = NativeCommand.queueFavorite(try FavoriteQueue(queueFavorite(), in: zone, next: true))
        try append.validate(before: before, current: favoriteQueueState(zone, uri: "fixture:two"))
        try next.validate(before: before, current: favoriteQueueState(zone, at: 30))
        #expect(throws: ContractError.self) { try next.validate(before: before, current: favoriteQueueState(zone, uri: "fixture:two")) }
    }

    @Test func receiptDescribesQueueInsertionNotImmediatePlayback() throws {
        let value = try FavoriteQueue(queueFavorite(), in: favoriteQueueZone(), next: false)
        let receipt = try value.receipt(favoriteQueueReply(), in: favoriteQueueZone())
        #expect(receipt.title == "Favorite queued" && !receipt.partial)
        #expect(receipt.detail.contains("3 entries") && receipt.detail.contains("current source was left unchanged"))
        let active = try value.receipt(favoriteQueueReply(["playing_queue": .bool(true)]), in: nil)
        #expect(active.detail.contains("not started or restarted"))
    }

    @Test func incompleteContradictoryAndOverflowingReceiptsAreNotAcknowledgements() throws {
        let value = try FavoriteQueue(queueFavorite(), in: favoriteQueueZone(), next: false)
        for change: [String: JSONValue] in [
            ["zone": .string("Other")], ["zone": .null], ["first": .integer(0)],
            ["count": .integer(0)], ["first": .integer(Int64(UInt32.max)), "count": .integer(2)],
            ["done": .string(" ")], ["count": .string("3")], ["playing_queue": .null]
        ] {
            #expect(throws: (any Error).self) { try value.receipt(favoriteQueueReply(change), in: nil) }
        }
        #expect(throws: (any Error).self) { try value.receipt(Data("{}".utf8), in: nil) }
        #expect(throws: (any Error).self) { try value.receipt(favoriteQueueReply(), in: favoriteQueueZone("S1")) }
        _ = try value.receipt(favoriteQueueReply(["first": .integer(Int64(UInt32.max)), "count": .integer(1)]), in: nil)
    }
}

private actor FavoriteQueueTransport: DaemonTransport {
    var requests: [URLRequest] = []
    var favorites = [queueFavorite()]
    var zone = favoriteQueueZone()
    var uri = "fixture:one"
    var favoriteReads = 0
    var heldRead: Int?
    var holdWrite = false
    var failFavorites = false
    var loseWrite = false
    var reply: Data?
    var waiting = false
    private var waiter: CheckedContinuation<Void, Never>?

    func set(home: String) { zone = favoriteQueueZone(home) }
    func replace(_ values: [Favorite]) { favorites = values }
    func changeTrack() { uri = "fixture:two" }
    func hold(read: Int? = nil, write: Bool = false) { heldRead = read; holdWrite = write }
    func fail(read: Bool = false, write: Bool = false, reply: Data? = nil) {
        failFavorites = read; loseWrite = write; self.reply = reply
    }
    func release() { waiter?.resume(); waiter = nil; waiting = false; heldRead = nil; holdWrite = false }
    var writes: [URLRequest] { requests.filter { $0.httpMethod == "POST" } }
    func send(_ request: URLRequest) async throws -> HTTPReply {
        requests.append(request)
        let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
        let path = request.url!.path
        if request.httpMethod == "POST" {
            guard path == "/queue" else { throw ContractError.invalid("Unexpected playback write") }
            if holdWrite { waiting = true; await withCheckedContinuation { waiter = $0 } }
            if loseWrite { throw URLError(.networkConnectionLost) }
            return HTTPReply(data: try reply ?? favoriteQueueReply(), status: 200)
        }
        if path == "/zones" { return HTTPReply(data: try encoder.encode([zone]), status: 200) }
        if path == "/zones/\(zone.id.address)/state" {
            return HTTPReply(data: try encoder.encode(favoriteQueueState(zone, uri: uri)), status: 200)
        }
        if path == "/favorites" {
            favoriteReads += 1
            if heldRead == favoriteReads { waiting = true; await withCheckedContinuation { waiter = $0 } }
            if failFavorites { throw URLError(.cannotConnectToHost) }
            return HTTPReply(data: try encoder.encode(favorites), status: 200)
        }
        throw ContractError.invalid("Unexpected read: \(path)")
    }
    nonisolated func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> {
        AsyncThrowingStream { $0.finish() }
    }
}
private func favoriteQueueClient(_ transport: FavoriteQueueTransport) throws -> DaemonClient {
    DaemonClient(endpoint: try DaemonEndpoint("https://music.example.test"), transport: transport)
}
private func favoriteQueueWait(_ predicate: @Sendable () async -> Bool) async throws {
    let deadline = ContinuousClock.now.advanced(by: .seconds(3))
    while !(await predicate()) {
        guard ContinuousClock.now < deadline else { throw ContractError.invalid("Favorite queue operation did not reach its gate") }
        try await Task.sleep(for: .milliseconds(2))
    }
}

@Suite("Favorite queue shared command integration") struct FavoriteQueueCommandTests {
    @Test(arguments: ["S1", "S2"], [false, true])
    func reviewsThenDispatchesOneNativeQueueRequest(home: String, next: Bool) async throws {
        let transport = FavoriteQueueTransport(); await transport.set(home: home)
        let zone = favoriteQueueZone(home), client = try favoriteQueueClient(transport)
        let session = CommandReviewSession(client: client)
        let command = NativeCommand.queueFavorite(try FavoriteQueue(queueFavorite(), in: zone, next: next))
        let review = try await session.prepare(command, in: zone)
        #expect(review.favorite == queueFavorite())
        #expect(await transport.writes.isEmpty)
        let receipt = try await session.execute(review)
        #expect(receipt.detail.contains("current source was left unchanged"))
        let requests = await transport.requests
        #expect(requests.map { $0.url!.path } == ["/zones", "/zones/Room@\(home)/state", "/favorites",
                                               "/zones", "/zones/Room@\(home)/state", "/favorites", "/queue"])
        for request in requests.filter({ $0.url!.path == "/favorites" }) {
            #expect(URLComponents(url: request.url!, resolvingAgainstBaseURL: false)?.queryItems == [URLQueryItem(name: "zone", value: "Room@\(home)")])
        }
        let request = try #require(requests.last)
        let body = try #require(request.httpBody)
        #expect(try DaemonClient.decode([String: JSONValue].self, from: body) == command.payload(in: zone).body)
        #expect(request.httpMethod == "POST" && request.value(forHTTPHeaderField: "Content-Type") == "application/json")
        await #expect(throws: (any Error).self) { try await session.execute(review) }
        #expect(await transport.writes.count == 1)
    }

    @Test func changedFavoriteIsRefusedBeforeReviewAndBeforeConfirmation() async throws {
        for beforeReview in [true, false] {
            let transport = FavoriteQueueTransport(), session = CommandReviewSession(client: try favoriteQueueClient(transport))
            let command = NativeCommand.queueFavorite(try FavoriteQueue(queueFavorite(), in: favoriteQueueZone(), next: false))
            if beforeReview {
                await transport.replace([queueFavorite(title: "Replacement")])
                await #expect(throws: (any Error).self) { try await session.prepare(command, in: favoriteQueueZone()) }
            } else {
                let review = try await session.prepare(command, in: favoriteQueueZone())
                await transport.replace([queueFavorite(title: "Replacement")])
                await #expect(throws: (any Error).self) { try await session.execute(review) }
            }
            #expect(await transport.writes.isEmpty)
        }
    }

    @Test func failedFavoriteReadIsNotPermissionToUseTheCachedFavorite() async throws {
        let transport = FavoriteQueueTransport(), session = CommandReviewSession(client: try favoriteQueueClient(transport))
        let command = NativeCommand.queueFavorite(try FavoriteQueue(queueFavorite(), in: favoriteQueueZone(), next: false))
        let review = try await session.prepare(command, in: favoriteQueueZone())
        await transport.fail(read: true)
        await #expect(throws: (any Error).self) { try await session.execute(review) }
        #expect(await transport.writes.isEmpty)
    }

    @Test func crossHouseholdCommandIsRejectedWithoutReadsOrWrites() async throws {
        let transport = FavoriteQueueTransport(), session = CommandReviewSession(client: try favoriteQueueClient(transport))
        let command = NativeCommand.queueFavorite(try FavoriteQueue(queueFavorite(), in: favoriteQueueZone("S1"), next: false))
        await #expect(throws: (any Error).self) { try await session.prepare(command, in: favoriteQueueZone("S2")) }
        #expect(await transport.requests.isEmpty)
    }

    @Test func cancellationDuringFinalFavoriteReadCannotSend() async throws {
        let transport = FavoriteQueueTransport(), session = CommandReviewSession(client: try favoriteQueueClient(transport))
        let command = NativeCommand.queueFavorite(try FavoriteQueue(queueFavorite(), in: favoriteQueueZone(), next: false))
        let review = try await session.prepare(command, in: favoriteQueueZone())
        await transport.hold(read: 2)
        let task = Task { try await session.execute(review) }
        try await favoriteQueueWait { await transport.waiting }
        await session.cancel(); await transport.release()
        await #expect(throws: (any Error).self) { try await task.value }
        #expect(await transport.writes.isEmpty)
    }

    @Test func queueAddOwnsTheExistingPlaybackGateUntilItsWriteDrains() async throws {
        let transport = FavoriteQueueTransport(), session = CommandReviewSession(client: try favoriteQueueClient(transport))
        let command = NativeCommand.queueFavorite(try FavoriteQueue(queueFavorite(), in: favoriteQueueZone(), next: false))
        let review = try await session.prepare(command, in: favoriteQueueZone())
        await transport.hold(write: true)
        let task = Task { try await session.execute(review) }
        try await favoriteQueueWait { await transport.waiting }
        await #expect(throws: (any Error).self) { try await session.prepare(.pause, in: favoriteQueueZone()) }
        await #expect(throws: (any Error).self) { try await session.execute(review) }
        await transport.release(); _ = try await task.value
        #expect(await transport.writes.count == 1)
    }

    @Test func trackAdvanceRejectsNextButAllowsAppendThroughTheSession() async throws {
        for next in [false, true] {
            let transport = FavoriteQueueTransport(), session = CommandReviewSession(client: try favoriteQueueClient(transport))
            let command = NativeCommand.queueFavorite(try FavoriteQueue(queueFavorite(), in: favoriteQueueZone(), next: next))
            let review = try await session.prepare(command, in: favoriteQueueZone())
            await transport.changeTrack()
            if next { await #expect(throws: (any Error).self) { try await session.execute(review) } }
            else { _ = try await session.execute(review) }
            #expect(await transport.writes.count == (next ? 0 : 1))
        }
    }

    @Test func lostAndContradictoryRepliesStayUncertainAndNeverRepeat() async throws {
        for reply in [nil, Data("{}".utf8), try favoriteQueueReply(["count": .integer(0)]), try favoriteQueueReply(["zone": .string("Other")])] {
            let transport = FavoriteQueueTransport(), session = CommandReviewSession(client: try favoriteQueueClient(transport))
            let command = NativeCommand.queueFavorite(try FavoriteQueue(queueFavorite(), in: favoriteQueueZone(), next: false))
            let review = try await session.prepare(command, in: favoriteQueueZone())
            await transport.fail(write: reply == nil, reply: reply)
            do { _ = try await session.execute(review); Issue.record("Expected an uncertain queue addition") }
            catch { guard case ClientError.uncertain = error else { Issue.record("Wrong error: \(error)"); continue } }
            await #expect(throws: (any Error).self) { try await session.execute(review) }
            #expect(await transport.writes.count == 1)
        }
    }
}
