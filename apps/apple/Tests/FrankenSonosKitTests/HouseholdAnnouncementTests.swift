import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

private func broadcastZone(_ lead: String, _ members: [String], home: String = "S2", state: String = "playing") -> Zone {
    Zone(household: home, coordinatorRoom: lead, members: members, transportState: state)
}
private func broadcastHouse(_ home: String = "S2") -> [Zone] {
    [broadcastZone("A", ["A", "B"], home: home), broadcastZone("C", ["C", "D"], home: home),
     broadcastZone("E", ["E"], home: home), broadcastZone("A", ["A", "C", "Other"], home: home == "S2" ? "S1" : "S2")]
}
private func broadcast(_ home: String = "S2", source: Announcement.Source = .speech("Dinner is ready")) throws -> Announcement {
    let house = broadcastHouse(home)
    return try Announcement(source: source, rooms: ["B", "C"], volume: 0, in: house[0], among: house)
}
private func broadcastWav() throws -> AnnouncementWav {
    let hex = "524946462600000057415645666d74201000000001000100401f0000803e00000200100064617461020000000000"
    let bytes = stride(from: 0, to: hex.count, by: 2).map { offset -> UInt8 in
        let start = hex.index(hex.startIndex, offsetBy: offset)
        return UInt8(hex[start..<hex.index(start, offsetBy: 2)], radix: 16)!
    }
    return try AnnouncementWav(data: Data(bytes), name: "Fixture.wav")
}

@Suite("Announcements across native household groups") struct HouseholdAnnouncementTests {
    @Test(arguments: ["S1", "S2"])
    func multiGroupSourcesUseOneExplicitHouseholdPayload(home: String) throws {
        let wav = try broadcastWav()
        for source in [Announcement.Source.speech(" Ready "), .chime(.bell), .wav(wav)] {
            let choice = try broadcast(home, source: source)
            let payload = try NativeCommand.announce(choice).payload(in: choice.group)
            #expect(payload.path == ["announce"] && payload.timeout == Announcement.timeout)
            #expect(payload.body["rooms"] == .array([.string("B@\(home)"), .string("C@\(home)")]))
            #expect(payload.body["volume"] == .integer(0))
            #expect(payload.body.count == 3)
            #expect(choice.affectedGroups.map(\.coordinatorRoom) == ["A", "C"])
            #expect(choice.explanation.contains("B@\(home)") && choice.explanation.contains("C@\(home)"))
            switch source {
            case .speech: #expect(payload.body["text"] == .string("Ready"))
            case .chime: #expect(payload.body["chime"] == .string("bell"))
            case .wav: #expect(payload.body["wav_base64"] == .string(wav.base64))
            }
        }
    }

    @Test func legacyInitializerDoesNotSilentlyBecomeWholeHousehold() throws {
        let zone = broadcastHouse()[0]
        let choice = try Announcement(source: .chime(.rise), rooms: ["B"], in: zone)
        #expect(choice.affectedGroups == [zone])
        #expect(throws: ContractError.self) { try Announcement(source: .chime(.rise), rooms: ["C"], in: zone) }
        #expect(throws: ContractError.self) { try Announcement(source: .chime(.rise), rooms: [], in: zone) }
    }

    @Test func scopeRejectsMissingAmbiguousForeignAndUnobservedRecipients() throws {
        let house = broadcastHouse(), zone = house[0]
        for rooms in [[], ["B", "B"], ["all"], ["Other"], ["C@S1"], [" C"]] as [[String]] {
            #expect(throws: ContractError.self) {
                try Announcement(source: .chime(.bell), rooms: rooms, in: zone, among: house)
            }
        }
        for topology in [[], [house[1]], [zone, zone], [zone, broadcastZone("C", ["B", "C"])]] {
            #expect(throws: ContractError.self) {
                try Announcement(source: .chime(.bell), rooms: ["B"], in: zone, among: topology)
            }
        }
    }

    @Test func everyTouchedGroupIsBoundButUnrelatedTopologyIsNot() throws {
        let choice = try broadcast(), original = broadcastHouse()
        for replacement in [[], [broadcastZone("C", ["C"])], [broadcastZone("D", ["C", "D"])],
                            [broadcastZone("C", ["C", "D", "New"])], [broadcastZone("C", ["C", "D"], home: "S1")]] {
            #expect(throws: ContractError.self) { try choice.validateTopology([original[0]] + replacement) }
        }
        let allowed = [broadcastZone("C", ["D", "C"], state: "paused"),
                       broadcastZone("A", ["B", "A"], state: "stopped"), broadcastZone("New", ["New"])]
        try choice.validateTopology(allowed)
        #expect(choice.affectedZones(in: allowed).count == 2)
        #expect(choice.affectedZones(in: original).flatMap(\.members).sorted() == ["A", "B", "C", "D"])
    }

    @Test func recipientGroupsWithoutTheNavigationGroupAreStillExplicit() throws {
        let house = broadcastHouse()
        let choice = try Announcement(source: .chime(.bell), rooms: ["D"], in: house[0], among: house)
        #expect(choice.affectedGroups == [house[1]])
        #expect(try choice.fields(in: house[0])["rooms"] == .array([.string("D@S2")]))
        #expect(choice.affectedZones(in: house).map(\.coordinatorRoom) == ["A", "C"])
        #expect(throws: ContractError.self) { try choice.fields(in: house[3]) }
    }

    @Test func topologyChecksKeepUnicodeIdentityBytes() throws {
        let plain = broadcastHouse()[0]
        let composed = broadcastZone("Caf\u{e9}", ["Caf\u{e9}", "D"])
        let decomposed = broadcastZone("Cafe\u{301}", ["Cafe\u{301}", "D"])
        let choice = try Announcement(source: .chime(.bell), rooms: ["D"], in: plain, among: [plain, composed])
        #expect(throws: ContractError.self) { try choice.validateTopology([plain, decomposed]) }
        #expect(throws: ContractError.self) {
            try Announcement(source: .chime(.bell), rooms: ["Cafe\u{301}"], in: plain, among: [plain, composed])
        }
    }

    @Test func composerStartsEmptyAndGroupAndHouseholdShortcutsStayDistinct() throws {
        let house = broadcastHouse()
        var form = AnnouncementForm(group: house[0], among: house)
        form.speech = "Ready"
        #expect(form.selectedRooms.isEmpty && form.availableGroups.count == 3)
        #expect(throws: ContractError.self) { try form.announcement(in: house[0]) }
        let selectedForeign = form.select("Other", selected: true)
        #expect(!selectedForeign)
        form.selectAll()
        #expect(form.selectedRooms == ["A", "B"])
        let selectedHouse = form.selectHousehold()
        #expect(selectedHouse)
        #expect(form.selectedRooms == ["A", "B", "C", "D", "E"])
        #expect(try form.announcement(in: house[0]).affectedGroups.count == 3)
        form.clearRooms()
        #expect(throws: ContractError.self) { try form.announcement(in: house[0]) }
        let selectedOtherGroup = form.select("C", selected: true)
        #expect(selectedOtherGroup)
        #expect(try form.announcement(in: house[0]).rooms == ["C"])
    }

    @Test func composerKeepsRecipientsForOrderingAndOtherHouseholdChangesOnly() throws {
        let house = broadcastHouse()
        var form = AnnouncementForm(group: house[0], among: house)
        form.speech = "Keep this"; form.select("B", selected: true); form.select("D", selected: true)
        let current = broadcastZone("A", ["B", "A"], state: "paused")
        let reordered = [house[2], broadcastZone("C", ["D", "C"], state: "stopped"), current]
        let harmlessChange = form.observe(current, among: reordered)
        #expect(!harmlessChange)
        #expect(form.selectedRooms == ["B", "D"] && form.notice == nil)
        let changed = [current, broadcastZone("C", ["C"]), house[2]]
        let meaningfulChange = form.observe(current, among: changed)
        #expect(meaningfulChange)
        #expect(form.selectedRooms.isEmpty && form.speech == "Keep this" && form.notice != nil)
        #expect(throws: ContractError.self) { try form.announcement(in: current) }
    }

    @Test func missingTopologyCannotFallBackToCachedGroupOrRecipients() throws {
        let house = broadcastHouse()
        var form = AnnouncementForm(group: house[0], among: house)
        form.speech = "Ready"; form.selectHousehold()
        let lostTopology = form.observe(house[0], among: [])
        #expect(lostTopology)
        form.selectAll()
        #expect(form.availableGroups.isEmpty && form.selectedRooms.isEmpty && !form.canSelectHousehold)
        let selectedRoom = form.select("B", selected: true), selectedHouse = form.selectHousehold()
        #expect(!selectedRoom && !selectedHouse)
        #expect(throws: ContractError.self) { try form.announcement(in: house[0]) }
        form.observe(house[0], among: house)
        #expect(form.selectedRooms.isEmpty)
    }

    @Test func householdChangeCancelsFileImportButRetainsLoadedAudioAndText() throws {
        let house = broadcastHouse(), wav = try broadcastWav()
        var form = AnnouncementForm(group: house[0], among: house)
        form.speech = "Keep message"; form.volume = 12; form.selectHousehold()
        let loaded = form.beginFileImport()
        let imported = form.finishFileImport(wav, id: loaded)
        #expect(imported)
        let regrouped = [house[0], broadcastZone("C", ["C"]), broadcastZone("D", ["D"]), house[2]]
        form.observe(house[0], among: regrouped)
        #expect(form.wav == wav && form.selectedRooms.isEmpty && form.speech == "Keep message" && form.volume == 12)
        let pending = form.beginFileImport()
        form.observe(house[0], among: house)
        let staleImport = form.finishFileImport(wav, id: pending)
        #expect(!form.fileLoading && !staleImport)
        #expect(throws: ContractError.self) { try form.announcement(in: house[0]) }
    }

    @Test func recipientLimitIsAtomicAcrossGroupsAndNeverTruncates() throws {
        let a = broadcastZone("A0", (0..<128).map { "A\($0)" })
        let b = broadcastZone("B0", (0..<129).map { "B\($0)" })
        let rooms = a.members + b.members
        #expect(throws: ContractError.self) { try Announcement(source: .chime(.bell), rooms: rooms, in: a, among: [a, b]) }
        #expect(try Announcement(source: .chime(.bell), rooms: Array(rooms.prefix(256)), in: a, among: [a, b]).rooms.count == 256)
        var form = AnnouncementForm(group: a, among: [a, b]); form.kind = .chime
        form.selectAll()
        let selectedHouse = form.selectHousehold()
        #expect(!selectedHouse && form.selectedRooms.count == 128)
        for room in b.members.prefix(128) { let selected = form.select(room, selected: true); #expect(selected) }
        let overflow = form.select("B128", selected: true)
        #expect(!overflow)
        #expect(try form.announcement(in: a).rooms.count == 256)
    }

    @Test func laterComposerEditsCannotRetargetReviewedHouseholdValue() throws {
        let house = broadcastHouse()
        var form = AnnouncementForm(group: house[0], among: house)
        form.speech = "Original"; form.select("B", selected: true); form.select("D", selected: true)
        let chosen = try form.announcement(in: house[0])
        form.selectHousehold(); form.speech = "Changed"; form.volume = 80
        form.observe(house[3], among: house)
        #expect(chosen.rooms == ["B", "D"] && chosen.volume == 35 && chosen.source == .speech("Original"))
        #expect(form.selectedRooms.isEmpty && form.group.household == "S1")
    }
}

private actor BroadcastTransport: DaemonTransport {
    var requests: [URLRequest] = []
    var topology: [Zone]
    var failed = Set<String>()
    var changedField: String?
    var elapsed: UInt32 = 10
    var topologyReads = 0
    var changeOnFinalTopology = false
    var loseReply = false, malformedReply = false, partialReply = false
    var blocked = false
    private var holdPath: String?
    private var gate: CheckedContinuation<Void, Never>?
    init(home: String = "S2") { topology = broadcastHouse(home) }
    func fail(_ room: String) { failed.insert(room) }
    func change(_ field: String) { changedField = field }
    func advance() { elapsed = 20 }
    func alterFinalTopology() { changeOnFinalTopology = true }
    func lose() { loseReply = true }
    func malformed() { malformedReply = true }
    func partial() { partialReply = true }
    func hold(_ path: String) { holdPath = path }
    func release() { let old = gate; gate = nil; blocked = false; old?.resume() }
    func writes() -> [URLRequest] { requests.filter { $0.httpMethod == "POST" } }
    func send(_ request: URLRequest) async throws -> HTTPReply {
        requests.append(request)
        let path = request.url!.path.removingPercentEncoding ?? request.url!.path
        if holdPath == path { holdPath = nil; blocked = true; await withCheckedContinuation { gate = $0 } }
        if path == "/announce" {
            if loseReply { throw URLError(.networkConnectionLost) }
            if malformedReply { return HTTPReply(data: Data("{}".utf8), status: 200) }
            let body = try JSONDecoder().decode([String: JSONValue].self, from: #require(request.httpBody))
            guard case .array(let targets)? = body["rooms"] else { throw ContractError.invalid("Missing test recipients") }
            let rooms = targets.compactMap { value -> String? in
                guard case .string(let room) = value else { return nil }; return room.components(separatedBy: "@")[0]
            }
            return try json(["done": "Announcement finished", "clean": !partialReply, "households": [
                ["lead": "fixture-lead", "rooms": rooms, "levels": rooms.map { ["room": $0, "volume": 0] as [String: Any] },
                 "outcome": "finished", "not_restored": partialReply ? ["D: restore was refused"] : []] as [String: Any]
            ]])
        }
        if path == "/zones" {
            topologyReads += 1
            if changeOnFinalTopology && topologyReads == 3 {
                let home = topology[0].household
                topology[1] = broadcastZone("C", ["C"], home: home)
                topology.append(broadcastZone("D", ["D"], home: home))
            }
            let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
            return HTTPReply(data: try encoder.encode(topology), status: 200)
        }
        if path.hasSuffix("/state") {
            let address = String(path.dropFirst("/zones/".count).dropLast("/state".count))
            guard let zone = topology.first(where: { $0.rooms.contains { $0.address == address } }) else {
                throw ContractError.invalid("Unknown test state target")
            }
            if failed.contains(address) { throw URLError(.cannotConnectToHost) }
            let changed = address == "D@\(topology[0].household)"
            let track = Track(uri: changed && changedField == "track" ? "fixture:other" : "fixture:music",
                              title: "Fixture", creator: nil, album: nil, durationSecs: 120, positionSecs: elapsed, queuePosition: 1)
            let state = ZoneState(zone: zone, transportState: "playing", volume: changed && changedField == "volume" ? 30 : 20,
                                  mute: changed && changedField == "mute", groupVolume: 20, track: track, shuffle: false, repeat: "off")
            let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
            return HTTPReply(data: try encoder.encode(state), status: 200)
        }
        throw ContractError.invalid("Unexpected test request: \(path)")
    }
    nonisolated func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> { AsyncThrowingStream { $0.finish() } }
    private func json(_ object: Any) throws -> HTTPReply { HTTPReply(data: try JSONSerialization.data(withJSONObject: object), status: 200) }
}
private func broadcastClient(_ transport: BroadcastTransport) throws -> DaemonClient {
    DaemonClient(endpoint: try DaemonEndpoint("https://music.example.test"), transport: transport)
}
private func broadcastWait(_ transport: BroadcastTransport) async throws {
    let deadline = ContinuousClock.now.advanced(by: .seconds(3))
    while !(await transport.blocked) {
        guard ContinuousClock.now < deadline else { throw URLError(.timedOut) }
        try await Task.sleep(for: .milliseconds(2))
    }
}

@Suite("Multi-group announcement shared review") struct HouseholdAnnouncementReviewTests {
    @Test(arguments: ["S1", "S2"])
    func checksEveryAffectedMemberThenSendsOneAnnouncement(home: String) async throws {
        let transport = BroadcastTransport(home: home), chosen = try broadcast(home)
        let session = try CommandReviewSession(client: broadcastClient(transport))
        await transport.fail("E@\(home)"); await transport.fail("A@\(home == "S2" ? "S1" : "S2")")
        let review = try await session.prepare(.announce(chosen), in: chosen.group)
        #expect(await transport.writes().isEmpty)
        await transport.advance()
        let result = try await session.execute(review)
        #expect(!result.partial && result.detail.contains("B, C"))
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        let writes = await transport.writes()
        #expect(writes.count == 1 && writes[0].url?.path == "/announce")
        let body = try JSONDecoder().decode([String: JSONValue].self, from: #require(writes.first?.httpBody))
        #expect(body == ["rooms": .array([.string("B@\(home)"), .string("C@\(home)")]), "volume": .integer(0), "text": .string("Dinner is ready")])
        let reads = await transport.requests.filter { $0.url?.path.hasSuffix("/state") == true }
        #expect(reads.count == 8)
        for name in ["A", "B", "C", "D"] { #expect(reads.filter { $0.url?.path.contains("/\(name)@\(home)/") == true }.count == 2) }
        #expect(await transport.topologyReads == 3)
    }

    @Test(arguments: [false, true])
    func unavailableUnselectedMemberOfRecipientGroupStopsTheWholeRequest(afterReview: Bool) async throws {
        let transport = BroadcastTransport(), chosen = try broadcast()
        let session = try CommandReviewSession(client: broadcastClient(transport))
        let review = afterReview ? try await session.prepare(.announce(chosen), in: chosen.group) : nil
        await transport.fail("D@S2")
        if let review { await #expect(throws: ContractError.self) { try await session.execute(review) } }
        else { await #expect(throws: ContractError.self) { try await session.prepare(.announce(chosen), in: chosen.group) } }
        #expect(await transport.writes().isEmpty)
    }

    @Test(arguments: ["volume", "mute", "track"])
    func changedNonRecipientGroupMemberInvalidatesTheReview(field: String) async throws {
        let transport = BroadcastTransport(), chosen = try broadcast()
        let session = try CommandReviewSession(client: broadcastClient(transport))
        let review = try await session.prepare(.announce(chosen), in: chosen.group)
        await transport.change(field)
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes().isEmpty)
    }

    @Test func finalClientTopologyReadRechecksNonAnchorGroups() async throws {
        let transport = BroadcastTransport(), chosen = try broadcast()
        let session = try CommandReviewSession(client: broadcastClient(transport))
        let review = try await session.prepare(.announce(chosen), in: chosen.group)
        await transport.alterFinalTopology()
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.topologyReads == 3)
        #expect(await transport.writes().isEmpty)
    }

    @Test func cancellationDuringOtherGroupReadRetainsTheSharedGateUntilDrain() async throws {
        let transport = BroadcastTransport(), chosen = try broadcast()
        let session = try CommandReviewSession(client: broadcastClient(transport))
        let review = try await session.prepare(.announce(chosen), in: chosen.group)
        await transport.hold("/zones/D@S2/state")
        let work = Task { try await session.execute(review) }
        try await broadcastWait(transport)
        await session.cancel()
        await #expect(throws: ContractError.self) { try await session.prepare(.pause, in: chosen.group) }
        await transport.release()
        await #expect(throws: CancellationError.self) { try await work.value }
        #expect(await transport.writes().isEmpty)
    }

    @Test func lostAndMalformedRepliesAreUncertainAndNeverRepeated() async throws {
        for malformed in [false, true] {
            let transport = BroadcastTransport(), chosen = try broadcast()
            let session = try CommandReviewSession(client: broadcastClient(transport))
            let review = try await session.prepare(.announce(chosen), in: chosen.group)
            if malformed { await transport.malformed() } else { await transport.lose() }
            do { _ = try await session.execute(review); Issue.record("Expected uncertain announcement") }
            catch ClientError.uncertain {} catch { Issue.record("Wrong error: \(error)") }
            await #expect(throws: ContractError.self) { try await session.execute(review) }
            #expect(await transport.writes().count == 1)
        }
    }

    @Test func sentAnnouncementCannotOverlapOrBeRecalledByCancellation() async throws {
        let transport = BroadcastTransport(), chosen = try broadcast()
        let session = try CommandReviewSession(client: broadcastClient(transport))
        let review = try await session.prepare(.announce(chosen), in: chosen.group)
        await transport.hold("/announce")
        let work = Task { try await session.execute(review) }
        try await broadcastWait(transport)
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        await #expect(throws: ContractError.self) { try await session.prepare(.pause, in: chosen.group) }
        await session.cancel(); work.cancel(); await transport.release()
        #expect(try await work.value.partial == false)
        #expect(await transport.writes().count == 1)
    }

    @Test func completedClipWithOtherGroupRestorationFailureRemainsPartial() async throws {
        let transport = BroadcastTransport(), chosen = try broadcast()
        let session = try CommandReviewSession(client: broadcastClient(transport))
        let review = try await session.prepare(.announce(chosen), in: chosen.group)
        await transport.partial()
        let result = try await session.execute(review)
        #expect(result.partial && result.detail.contains("D: restore was refused"))
        #expect(await transport.writes().count == 1)
    }
}
