import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

private func targetZone(_ room: String = "Den", home: String = "S2") -> Zone {
    Zone(household: home, coordinatorRoom: room,
         members: room == "Den" ? ["Den", "Member"] : [room], transportState: "playing")
}
private func targetPlaylist() -> SonosPlaylist {
    SonosPlaylist(id: "SQ:7", title: "Native source", artUri: nil)
}
private func targetPageData(revision: String = "native:r1") throws -> Data {
    let items: [JSONValue] = (1...3).map { .object([
        "position": .integer(Int64($0)), "title": .string("Native \($0)"),
        "uri": .string("fixture:native\($0)"), "duration_secs": .integer(180)
    ]) }
    return try JSONEncoder().encode([
        "zone": JSONValue.string("Den"),
        "playlist": .object(["id": .string("SQ:7"), "title": .string("Native source")]),
        "revision": .string(revision), "total": .integer(3), "offset": .integer(0),
        "next_offset": .null, "items": .array(items)
    ])
}
private func targetDraft(home: String = "S2") throws -> SonosPlaylistTracksDraft {
    let page = try DaemonClient.decode(SonosPlaylistContents.self, from: targetPageData())
    let request = try SonosPlaylistContentsRequest(playlist: targetPlaylist(), in: targetZone(home: home))
    var draft = SonosPlaylistTracksDraft()
    for index in [2, 0, 2] { try draft.append(page.items[index], from: page, request: request) }
    return draft
}

/// Same room names in both households deliberately expose any accidental
/// source retargeting. All identities, tracks and addresses are synthetic.
private actor TargetsTransport: DaemonTransport {
    let home: String
    var requests: [URLRequest] = []
    var contentsZones: [String] = []
    var blocked = false
    private var queues: [String: Int] = [:]
    private var tracks: [String: UInt32] = [:]
    private var audienceRevision = String(repeating: "a", count: 64)
    private var savedRevision = "native:r1"
    private var audienceError: String?
    private var receiptStyle = "complete"
    private var driftOnSource = false
    private var hold: String?
    private var gate: CheckedContinuation<Void, Never>?
    nonisolated let stream: AsyncThrowingStream<EventSignal, Error>
    nonisolated let output: AsyncThrowingStream<EventSignal, Error>.Continuation

    init(home: String = "S2") {
        self.home = home
        let pair = AsyncThrowingStream<EventSignal, Error>.makeStream()
        stream = pair.stream; output = pair.continuation
    }
    var writes: [URLRequest] { requests.filter { $0.httpMethod == "POST" } }
    func changeQueue(_ room: String = "Office") { queues[room, default: 1] += 1 }
    func advance(_ room: String = "Office") { tracks[room] = 2 }
    func changeAudience() { audienceRevision = String(repeating: "b", count: 64) }
    func changeSource() { savedRevision = "native:r2" }
    func invalidateAudience(_ kind: String) { audienceError = kind }
    func respond(_ style: String) { receiptStyle = style }
    func driftDuringSource() { driftOnSource = true }
    func holdNext(_ boundary: String) { hold = boundary }
    func release() { blocked = false; gate?.resume(); gate = nil }

    private func waitIfHeld(_ boundary: String) async {
        if hold == boundary {
            hold = nil; blocked = true
            await withCheckedContinuation { gate = $0 }
        }
    }
    private func player(_ room: String, home: String) -> String { "fixture-\(home)-\(room)" }
    private func room(_ name: String, coordinator: String, home: String) -> JSONValue {
        .object(["room": .string(name), "household": .string(home),
                 "player": .string(player(name, home: home)), "coordinator": .string(player(coordinator, home: home))])
    }
    private func group(_ name: String, home: String) -> JSONValue {
        .object(["household": .string(home), "coordinator": .string(player(name, home: home)),
                 "coordinator_room": .string(name),
                 "members": .array(targetZone(name, home: home).members.map { room($0, coordinator: name, home: home) })])
    }
    private func audience(_ target: String) throws -> Data {
        let selected = target.hasPrefix("Office@") ? ["Office"] : ["Den", "Office"]
        let selectedHome = audienceError == "household" ? (home == "S2" ? "S1" : "S2") : home
        var groups = selected.map { group($0, home: selectedHome) }
        var rooms = selected.map { room($0 == "Den" ? "Member" : $0, coordinator: $0, home: selectedHome) }
        if audienceError == "duplicate" { groups.append(groups[0]) }
        if audienceError == "missing" { rooms.removeLast() }
        if audienceError == "member" {
            groups[0] = .object(["household": .string(home), "coordinator": .string(player("Den", home: home)),
                "coordinator_room": .string("Den"), "members": .array([room("Den", coordinator: "Den", home: home)])])
        }
        return try JSONEncoder().encode([
            "target": JSONValue.string(audienceError == "target" ? "Different alias" : target),
            "revision": .string(audienceError == "revision" ? "not-a-revision" : audienceRevision),
            "matched": .string(target.hasPrefix("all@") ? "everywhere" : target.hasPrefix("Office@") ? "room" : "alias"),
            "rooms": .array(rooms), "groups": .array(groups)
        ])
    }
    private func problem(_ uncertain: Bool = false) -> JSONValue {
        .object(["code": .string(uncertain ? "OUTCOME_UNCERTAIN" : "UPNP_ERROR"),
                 "detail": .string(uncertain ? "The speaker reply was lost." : "The speaker rejected this change."),
                 "hint": .string("Inspect this original group."), "retryable": .bool(false), "suggestions": .array([])])
    }
    private func receipt(_ request: URLRequest) throws -> Data {
        let body = try JSONDecoder().decode([String: JSONValue].self, from: request.httpBody!)
        guard case .array(let selected)? = body["positions"], case .string(let mode)? = body["mode"],
              case .string(let target)? = body["target"] else { throw ContractError.invalid("Missing explicit native selection") }
        let positions = selected.compactMap { value -> Int64? in
            if case .integer(let n) = value { n } else { nil }
        }
        let groups = target.hasPrefix("Office@") ? ["Office"] : ["Den", "Office"]
        var report: [String: JSONValue] = [
            "done": .string("Native selection handled"), "zone": .string(groups.count == 1 ? groups[0] : target),
            "playlist": .string("SQ:7"), "positions": .array(selected),
            "titles": .array(positions.map { .string("Native \($0)") }),
            "mode": .string(mode), "changed": .bool(true), "notes": .array([])
        ]
        if mode == "play" {
            if groups.count > 1 {
                var results: [JSONValue] = groups.enumerated().map { index, name in
                    let result: JSONValue
                    if receiptStyle == "unattempted" {
                        result = index == 0
                            ? .object(["status": .string("failed"), "error": problem()])
                            : .object(["status": .string("not_attempted")])
                    } else if index == 1 && receiptStyle == "partial" {
                        result = .object(["status": .string("failed"), "error": problem(true)])
                    } else {
                        result = .object(["status": .string("succeeded"), "done": .string("Played selected occurrences"),
                                          "changed": .bool(true), "notes": .array([])])
                    }
                    return .object(["household": .string(home),
                        "target": .string(receiptStyle == "foreign" && index == 1 ? "foreign-player" : player(name, home: home)),
                        "zone": .string(name), "result": result])
                }
                if receiptStyle == "missing" { results.removeLast() }
                if receiptStyle == "duplicate" { results[1] = results[0] }
                report["batch"] = .object(["complete": .bool(receiptStyle == "complete"), "results": .array(results)])
                if receiptStyle == "unattempted" { report["changed"] = .bool(false) }
            }
        } else {
            var added: [JSONValue] = [], failed: [JSONValue] = []
            for (index, name) in groups.enumerated() {
                if receiptStyle == "uncertain" || (receiptStyle == "partial" && index == 1) {
                    failed.append(.object(["household": .string(home), "coordinator": .string(player(name, home: home)),
                                           "zone": .string(name), "error": problem(receiptStyle == "uncertain")]))
                } else {
                    added.append(.object(["household": .string(home),
                        "coordinator": .string(receiptStyle == "foreign" && index == 1 ? "foreign-player" : player(name, home: home)),
                        "queue": .object(["done": .string("Added selected occurrences"), "zone": .string(name),
                            "first": .integer(3), "count": .integer(Int64(positions.count)), "playing_queue": .bool(false),
                            "notes": .array([])])]))
                }
            }
            if receiptStyle == "missing" { added.removeLast() }
            if receiptStyle == "duplicate" { added[1] = added[0] }
            let complete = receiptStyle == "complete"
            report["queue_batch"] = .object(["done": .string("Native additions handled"), "complete": .bool(complete),
                "added": .array(added), "failed": .array(failed), "logged": .bool(complete), "undoable": .bool(complete)])
            if added.count == 1 && failed.isEmpty {
                report["zone"] = .string(groups[0]); report["first"] = .integer(3)
                report["count"] = .integer(Int64(positions.count)); report["playing_queue"] = .bool(false)
            } else { report["zone"] = .string(target) }
        }
        return try JSONEncoder().encode(report)
    }

    private func failureReceipt() throws -> Data {
        var results: [JSONValue] = ["Den", "Office"].enumerated().map { index, name in
            let result: JSONValue
            if receiptStyle == "http_success" {
                result = .object(["status": .string("succeeded"), "done": .string("Played selected occurrences"),
                                  "changed": .bool(true)])
            } else if index == 0 {
                result = .object(["status": .string("failed"), "error": problem(receiptStyle == "http_uncertain")])
            } else { result = .object(["status": .string("not_attempted")]) }
            return .object(["household": .string(home),
                "target": .string(receiptStyle == "http_foreign" && index == 0 ? "foreign-player" : player(name, home: home)),
                "zone": .string(name), "result": result])
        }
        if receiptStyle == "http_missing" { results.removeLast() }
        if receiptStyle == "http_duplicate" { results[1] = results[0] }
        return try JSONEncoder().encode([
            "code": JSONValue.string(receiptStyle == "http_uncertain" ? "OUTCOME_UNCERTAIN" : "UPNP_ERROR"),
            "detail": .string("Native selection did not complete."),
            "hint": .string("Inspect the reviewed groups."), "retryable": .bool(false),
            "batch": .object(["complete": .bool(receiptStyle == "http_complete"), "results": .array(results)])
        ])
    }

    func send(_ request: URLRequest) async throws -> HTTPReply {
        requests.append(request)
        let path = request.url!.path.removingPercentEncoding ?? request.url!.path
        let query = URLComponents(url: request.url!, resolvingAgainstBaseURL: false)?.queryItems ?? []
        let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
        if request.httpMethod == "POST" {
            #expect(path == "/sonos/playlists/tracks")
            await waitIfHeld("write")
            if receiptStyle == "lost" { throw URLError(.networkConnectionLost) }
            if receiptStyle.hasPrefix("http_") { return HTTPReply(data: try failureReceipt(), status: 503) }
            return HTTPReply(data: try receipt(request), status: 200)
        }
        if path == "/zones" {
            let other = home == "S2" ? "S1" : "S2"
            let zones = [targetZone(home: home), targetZone("Office", home: home),
                         targetZone(home: other), targetZone("Office", home: other)]
            return HTTPReply(data: try encoder.encode(zones), status: 200)
        }
        if path == "/rooms/resolve" {
            await waitIfHeld("audience")
            let target = query.first(where: { $0.name == "target" })!.value!
            return HTTPReply(data: try audience(target), status: 200)
        }
        if path.hasSuffix("/state") || path.hasSuffix("/queue") {
            let selectedHome = path.contains("@S1/") ? "S1" : "S2"
            let name = path.contains("Office@") ? "Office" : "Den"
            let zone = targetZone(name, home: selectedHome), current = tracks[name] ?? 1
            if path.hasSuffix("/state") {
                let state = ZoneState(zone: zone, transportState: "playing", volume: 20, mute: false, groupVolume: 20,
                    track: Track(uri: "fixture:\(name)-\(current)", title: "Current \(name)", creator: nil,
                        album: nil, durationSecs: 180, positionSecs: 17, queuePosition: current),
                    shuffle: false, repeat: "off")
                return HTTPReply(data: try encoder.encode(state), status: 200)
            }
            let limit = Int(query.first(where: { $0.name == "limit" })?.value ?? "25") ?? 25
            let items: [JSONValue] = (1...min(2, limit)).map { .object([
                "position": .integer(Int64($0)), "title": .string("Queue \($0)"), "uri": .string("fixture:queue\($0)")
            ]) }
            return HTTPReply(data: try JSONEncoder().encode([
                "zone": JSONValue.string(name), "total": .integer(2),
                "revision": .string("queue:\(name):\(queues[name] ?? 1)"),
                "playing": .integer(Int64(current)), "items": .array(items)
            ]), status: 200)
        }
        if path == "/sonos/playlists" {
            return HTTPReply(data: try encoder.encode([targetPlaylist()]), status: 200)
        }
        if path == "/sonos/playlists/contents" {
            contentsZones.append(query.first(where: { $0.name == "zone" })?.value ?? "")
            await waitIfHeld("source")
            if driftOnSource { driftOnSource = false; changeQueue() }
            return HTTPReply(data: try targetPageData(revision: savedRevision), status: 200)
        }
        throw ContractError.invalid("Unexpected native destination route: \(path)")
    }
    nonisolated func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> {
        output.yield(.connected); return stream
    }
}
private func targetClient(_ transport: TargetsTransport) throws -> DaemonClient {
    DaemonClient(endpoint: try DaemonEndpoint("https://music.example.test"), transport: transport)
}
private func targetsWait(_ condition: @Sendable () async -> Bool) async throws {
    let deadline = ContinuousClock.now.advanced(by: .seconds(3))
    while !(await condition()) {
        guard ContinuousClock.now < deadline else { throw ContractError.invalid("Destination fixture did not reach its boundary") }
        try await Task.sleep(for: .milliseconds(2))
    }
}
@MainActor private func targetModelWait(_ condition: () -> Bool) async throws {
    let deadline = ContinuousClock.now.advanced(by: .seconds(3))
    while !condition() {
        guard ContinuousClock.now < deadline else { throw ContractError.invalid("Destination model did not reach its boundary") }
        try await Task.sleep(for: .milliseconds(2))
    }
}

@Suite("Native saved-playlist destination audience") struct SonosPlaylistTargetsTests {
    @Test func destinationEditingPreservesOccurrencesButSourceChangesRetireBoth() throws {
        var draft = try targetDraft()
        let token = draft.id, choices = draft.choices
        draft.setDestination("W"); draft.setDestination("Weekend@S2")
        #expect(draft.id == token && draft.choices == choices && draft.target == "Weekend@S2")
        let page = try DaemonClient.decode(SonosPlaylistContents.self, from: targetPageData(revision: "native:r2"))
        try draft.observe(page, request: SonosPlaylistContentsRequest(playlist: targetPlaylist(), in: targetZone()))
        #expect(draft.isEmpty && draft.target.isEmpty)
    }

    @Test(arguments: ["S1", "S2"], [SonosPlaylistAction.play, .next, .append])
    func aliasAudiencePreservesSourceOrderAndUsesOneWrite(home: String, action: SonosPlaylistAction) async throws {
        let transport = TargetsTransport(home: home), session = CommandReviewSession(client: try targetClient(transport))
        let target = "Weekend@\(home)"
        let selection = try SonosPlaylistTracksSelection(targetDraft(home: home), action: action, target: target)
        let review = try await session.prepare(.sonosPlaylistTracks(selection), in: targetZone(home: home))
        #expect(review.playlistDestinations.map(\.zone.id) == [targetZone(home: home).id, targetZone("Office", home: home).id])
        #expect(review.playlistDestinations[0].zone.members == ["Den", "Member"])
        #expect(review.playlistDestinations.allSatisfy { $0.queueTotal == 2 && $0.playing == 1 })
        #expect(review.command.destinationSummary?.contains("Den + Member · \(home)") == true)
        #expect(await transport.writes.isEmpty)
        let receipt = try await session.execute(review)
        #expect(!receipt.partial)
        #expect(receipt.detail.contains("Den · \(home)") && receipt.detail.contains("Office · \(home)"))
        let writes = await transport.writes
        #expect(writes.count == 1)
        let body = try JSONDecoder().decode([String: JSONValue].self, from: #require(writes.first?.httpBody))
        #expect(body == [
            "zone": .string("Den@\(home)"), "playlist": .string("SQ:7"), "expected_revision": .string("native:r1"),
            "positions": .array([.integer(3), .integer(1), .integer(3)]), "mode": .string(action.rawValue),
            "target": .string(target), "expected_target_revision": .string(String(repeating: "a", count: 64))
        ])
        #expect(await transport.contentsZones == ["Den@\(home)", "Den@\(home)"])
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes.count == 1)
    }

    @Test(arguments: [SonosPlaylistAction.play, .next, .append])
    func differentSingleRoomBindsItsQueueAndIgnoresUnrelatedSourcePlayback(action: SonosPlaylistAction) async throws {
        let transport = TargetsTransport(), session = CommandReviewSession(client: try targetClient(transport))
        let selection = try SonosPlaylistTracksSelection(targetDraft(), action: action, target: "Office@S2")
        let review = try await session.prepare(.sonosPlaylistTracks(selection), in: targetZone())
        await transport.changeQueue("Den"); await transport.advance("Den")
        let receipt = try await session.execute(review)
        #expect(!receipt.partial)
        let writes = await transport.writes
        let body = try JSONDecoder().decode([String: JSONValue].self, from: #require(writes.first?.httpBody))
        #expect(body["expected_queue_revision"] == .string("queue:Office:1"))
        #expect(body["zone"] == .string("Den@S2") && body["target"] == .string("Office@S2"))
        #expect(await transport.contentsZones == ["Den@S2", "Den@S2"])
    }

    @Test(arguments: ["source", "queue", "playback", "audience"])
    func changedProofPreventsEveryDestinationWrite(change: String) async throws {
        let transport = TargetsTransport(), session = CommandReviewSession(client: try targetClient(transport))
        let selection = try SonosPlaylistTracksSelection(targetDraft(), action: .next, target: "all@S2")
        let review = try await session.prepare(.sonosPlaylistTracks(selection), in: targetZone())
        switch change {
        case "source": await transport.changeSource()
        case "queue": await transport.changeQueue()
        case "playback": await transport.advance()
        default: await transport.changeAudience()
        }
        await #expect(throws: (any Error).self) { try await session.execute(review) }
        #expect(await transport.writes.isEmpty)
    }

    @Test func appendAllowsDestinationProgressAndSlowSourceReadsRecheckDestinations() async throws {
        let transport = TargetsTransport(), session = CommandReviewSession(client: try targetClient(transport))
        let selection = try SonosPlaylistTracksSelection(targetDraft(), action: .append, target: "all@S2")
        let review = try await session.prepare(.sonosPlaylistTracks(selection), in: targetZone())
        await transport.advance()
        _ = try await session.execute(review)
        #expect(await transport.writes.count == 1)

        let drifting = TargetsTransport(), fresh = CommandReviewSession(client: try targetClient(drifting))
        await drifting.driftDuringSource()
        await #expect(throws: ContractError.self) { try await fresh.prepare(.sonosPlaylistTracks(selection), in: targetZone()) }
        #expect(await drifting.writes.isEmpty)
    }

    @Test(arguments: ["household", "duplicate", "missing", "member", "target", "revision"])
    func invalidAudienceStopsBeforeSourceResolution(kind: String) async throws {
        let transport = TargetsTransport(), session = CommandReviewSession(client: try targetClient(transport))
        await transport.invalidateAudience(kind)
        let selection = try SonosPlaylistTracksSelection(targetDraft(), action: .play, target: "all@S2")
        await #expect(throws: (any Error).self) { try await session.prepare(.sonosPlaylistTracks(selection), in: targetZone()) }
        #expect(await transport.contentsZones.isEmpty)
        #expect(await transport.writes.isEmpty)
    }

    @Test(arguments: ["audience", "source"])
    func cancellationDuringReadCannotSendOrReleaseBusyWriter(boundary: String) async throws {
        let transport = TargetsTransport(), session = CommandReviewSession(client: try targetClient(transport))
        let selection = try SonosPlaylistTracksSelection(targetDraft(), action: .play, target: "all@S2")
        let review = try await session.prepare(.sonosPlaylistTracks(selection), in: targetZone())
        await transport.holdNext(boundary)
        let task = Task { try await session.execute(review) }
        try await targetsWait { await transport.blocked }
        task.cancel(); await session.cancel()
        await #expect(throws: ContractError.self) { try await session.prepare(.pause, in: targetZone()) }
        await transport.release()
        await #expect(throws: CancellationError.self) { try await task.value }
        #expect(await transport.writes.isEmpty)
    }

    @Test(arguments: [SonosPlaylistAction.play, .next, .append])
    func partialReceiptsRetainSuccessfulAndFailedGroups(action: SonosPlaylistAction) async throws {
        let transport = TargetsTransport(), session = CommandReviewSession(client: try targetClient(transport))
        let selection = try SonosPlaylistTracksSelection(targetDraft(), action: action, target: "all@S2")
        let review = try await session.prepare(.sonosPlaylistTracks(selection), in: targetZone())
        await transport.respond("partial")
        let receipt = try await session.execute(review)
        #expect(receipt.partial)
        #expect(receipt.detail.contains("Den · S2") && receipt.detail.contains("Office · S2"))
        #expect(receipt.detail.contains("Inspect this original group."))
        #expect(receipt.detail.contains(action == .play ? "Outcome uncertain" : "Failed"))
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes.count == 1)
    }

    @Test(arguments: ["http_failed", "http_uncertain"])
    func nonSuccessPlaybackResponseKeepsEachOriginalGroupWithoutClaimingSuccess(style: String) async throws {
        let transport = TargetsTransport(), session = CommandReviewSession(client: try targetClient(transport))
        let selection = try SonosPlaylistTracksSelection(targetDraft(), action: .play, target: "all@S2")
        let review = try await session.prepare(.sonosPlaylistTracks(selection), in: targetZone())
        await transport.respond(style)
        do { _ = try await session.execute(review); Issue.record("HTTP failure became a confirmed receipt") }
        catch ClientError.uncertain(let detail) {
            #expect(detail.contains("Native selection did not complete."))
            #expect(detail.contains("Den · S2") && detail.contains("Office · S2"))
            #expect(detail.contains(style == "http_uncertain" ? "Outcome uncertain" : "Failed"))
            #expect(detail.contains("Not attempted"))
            #expect(detail.contains("Inspect the reviewed groups."))
        }
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes.count == 1)
    }

    @Test(arguments: ["http_missing", "http_duplicate", "http_foreign", "http_complete", "http_success"])
    func malformedErrorBatchCannotAttributeUnprovedGroupOutcomes(style: String) async throws {
        let transport = TargetsTransport(), session = CommandReviewSession(client: try targetClient(transport))
        let selection = try SonosPlaylistTracksSelection(targetDraft(), action: .play, target: "all@S2")
        let review = try await session.prepare(.sonosPlaylistTracks(selection), in: targetZone())
        await transport.respond(style)
        do { _ = try await session.execute(review); Issue.record("Malformed error batch was accepted") }
        catch ClientError.uncertain(let detail) {
            #expect(!detail.contains("Den · S2") && !detail.contains("Office · S2"))
        }
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes.count == 1)
    }

    @Test func unattemptedPlayAndZeroConfirmedUncertainQueueRemainPartial() async throws {
        for action in [SonosPlaylistAction.play, .append] {
            let transport = TargetsTransport(), session = CommandReviewSession(client: try targetClient(transport))
            let target = action == .play ? "all@S2" : "Office@S2"
            let selection = try SonosPlaylistTracksSelection(targetDraft(), action: action, target: target)
            let review = try await session.prepare(.sonosPlaylistTracks(selection), in: targetZone())
            await transport.respond(action == .play ? "unattempted" : "uncertain")
            let receipt = try await session.execute(review)
            #expect(receipt.partial)
            #expect(receipt.detail.contains(action == .play ? "Not attempted" : "Outcome uncertain"))
            if action == .append {
                #expect(!receipt.detail.contains("Added 3"))
                #expect(receipt.detail.contains("No aggregate undo"))
            }
            #expect(await transport.writes.count == 1)
        }
    }

    @Test(arguments: ["lost", "missing", "duplicate", "foreign"], [SonosPlaylistAction.play, .append])
    func incompleteOrForeignReceiptsAreUncertainWithoutReplay(kind: String, action: SonosPlaylistAction) async throws {
        let transport = TargetsTransport(), session = CommandReviewSession(client: try targetClient(transport))
        let selection = try SonosPlaylistTracksSelection(targetDraft(), action: action, target: "all@S2")
        let review = try await session.prepare(.sonosPlaylistTracks(selection), in: targetZone())
        await transport.respond(kind)
        do { _ = try await session.execute(review); Issue.record("Unbound destination receipt was accepted") }
        catch ClientError.uncertain { }
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes.count == 1)
    }

    @MainActor @Test func screenKeepsSourceSeparateAndRetainsLateAudienceAfterNavigationAndSuspension() async throws {
        let transport = TargetsTransport(), model = RemoteModel(client: try targetClient(transport))
        model.resume(); defer { model.stop() }
        try await targetModelWait { model.selectedZone != nil && !model.panelLoading }
        model.show(.sonosPlaylists)
        try await targetModelWait { !model.panelLoading && model.sonosPlaylists.count == 1 }
        let zone = try #require(model.selectedZone)
        model.openSonosPlaylist(targetPlaylist(), in: zone)
        try await targetModelWait { !model.panelLoading && model.sonosPlaylistContents != nil }
        let page = try #require(model.sonosPlaylistContents), request = try #require(model.sonosPlaylistRequest)
        model.addSonosPlaylistTrack(page.items[2], from: page, request: request, in: zone)
        let token = model.sonosPlaylistTracks.id
        model.changeSonosPlaylistDestination("Week", selection: token)
        model.changeSonosPlaylistDestination("Weekend@S2", selection: token)
        #expect(model.sonosPlaylistTracks.target == "Weekend@S2")
        #expect(model.selectedZone?.sameGroup(as: zone) == true)
        model.prepareSonosPlaylistTracks(.append, selection: token, in: zone)
        try await targetModelWait { model.review != nil && model.actionPhase == .idle }
        let review = try #require(model.review)
        #expect(review.playlistDestinations.count == 2 && review.zone.sameGroup(as: zone))
        model.changeSonosPlaylistDestination("Office@S2", selection: token)
        #expect(model.sonosPlaylistTracks.target == "Weekend@S2")
        #expect(await transport.writes.isEmpty)
        await transport.holdNext("write")
        model.confirm(review.id)
        #expect(model.sonosPlaylistTracks.isEmpty && model.sonosPlaylistTracks.target.isEmpty)
        try await targetsWait { await transport.blocked }
        model.select(targetZone("Office", home: "S1").id)
        model.suspend(); await transport.release()
        try await targetModelWait { model.result != nil && model.actionPhase == .idle }
        #expect(model.result?.kind == .confirmed)
        #expect(model.result?.target == zone.id)
        #expect(model.result?.destination == "Den + Member · S2\nOffice · S2")
        model.confirm(review.id)
        #expect(await transport.writes.count == 1)
    }
}
