import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

private let listeningGroup = Zone(household: "HOUSE_A", coordinatorRoom: "Studio", members: ["Studio", "Kitchen"], transportState: "paused")
private let listeningTone = SpeakerTone(room: "Kitchen", bass: 0, treble: 0, loudness: true)
private let listeningOrder = QueuePlaybackOrder(zone: "Studio", mode: .normal, queueActive: true, djActive: false)

private actor ListeningDaemon: DaemonTransport {
    struct Call: Sendable { let path: String; let method: String; let body: [String: JSONValue] }
    var calls: [Call] = []
    var group = listeningGroup
    var bass = 0, treble = 0
    var loudness = true, queue = true, dj = false
    var mode = QueuePlaybackMode.normal
    var currentTrack = "fixture:trackA"
    var failure: String?
    var badReply = false, recovered = false
    var holdPath: String?, holdMethod = "GET"
    var waiting: CheckedContinuation<Void, Never>?
    var writes: [Call] { calls.filter { $0.method == "POST" } }
    func setTone(bass: Int = 0, treble: Int = 0, loudness: Bool = true) {
        self.bass = bass; self.treble = treble; self.loudness = loudness
    }
    func setOrder(_ mode: QueuePlaybackMode = .normal, queue: Bool = true, dj: Bool = false) {
        self.mode = mode; self.queue = queue; self.dj = dj
    }
    func setGroup(_ group: Zone) { self.group = group }
    func advanceTrack() { currentTrack = "fixture:trackB" }
    func fail(_ path: String?) { failure = path }
    func malformed() { badReply = true }
    func recover() { recovered = true }
    func hold(_ path: String, method: String = "GET") { holdPath = path; holdMethod = method }
    func release() { waiting?.resume(); waiting = nil }
    nonisolated func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> {
        AsyncThrowingStream { $0.finish() }
    }
    func send(_ request: URLRequest) async throws -> HTTPReply {
        let path = request.url!.path, method = request.httpMethod ?? "GET"
        let body = try request.httpBody.map { try JSONDecoder().decode([String: JSONValue].self, from: $0) } ?? [:]
        calls.append(Call(path: path, method: method, body: body))
        if holdPath == path && holdMethod == method {
            holdPath = nil
            await withCheckedContinuation { waiting = $0 }
        }
        if failure == path { throw ContractError.invalid("fixture transport failure") }
        let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
        func reply<T: Encodable>(_ value: T) throws -> HTTPReply {
            HTTPReply(data: try encoder.encode(value), status: 200)
        }
        func tone() -> JSONValue {
            .object(["room": .string("Kitchen"), "bass": .integer(Int64(bass)), "treble": .integer(Int64(treble)), "loudness": .bool(loudness)])
        }
        func order() -> JSONValue {
            .object(["zone": .string(group.coordinatorRoom), "mode": .string(mode.rawValue), "queue_active": .bool(queue), "dj_active": .bool(dj)])
        }
        if method == "POST" {
            if badReply { return HTTPReply(data: Data("null".utf8), status: 200) }
            if path == "/eq" {
                if case .integer(let value) = body["bass"] { bass = Int(value) }
                if case .integer(let value) = body["treble"] { treble = Int(value) }
                if case .bool(let value) = body["loudness"] { loudness = value }
                return try reply(["done": JSONValue.string("Tone applied"), "eq": tone()])
            }
            if path == "/play-mode" {
                if case .string(let value) = body["mode"], let selected = QueuePlaybackMode(rawValue: value) { mode = selected }
                return try reply(["done": JSONValue.string("Mode applied"), "state": order(), "changed": .bool(true), "recovered": .bool(recovered)])
            }
            return try reply(["done": JSONValue.string("Changed"), "changed": .bool(true)])
        }
        if path == "/zones" { return try reply([group]) }
        if path.hasSuffix("/state") {
            return try reply(ZoneState(zone: group, transportState: "paused", volume: 20, mute: false, groupVolume: 20,
                track: Track(uri: currentTrack, title: "Synthetic track", creator: nil, album: nil,
                             durationSecs: 240, positionSecs: 20, queuePosition: 1), shuffle: false, repeat: "off"))
        }
        if path.hasSuffix("/eq") { return try reply(tone()) }
        if path.hasSuffix("/play-mode") { return try reply(order()) }
        throw ContractError.invalid("Unexpected test request: \(method) \(path)")
    }
}
private func listeningClient(_ transport: ListeningDaemon) throws -> DaemonClient {
    try DaemonClient(endpoint: DaemonEndpoint("https://daemon.invalid"), transport: transport)
}
private func listeningToneCommand() throws -> NativeCommand {
    .listeningSettings(.speaker(try SpeakerToneChange(target: RoomID(household: "HOUSE_A", room: "Kitchen"),
        before: listeningTone, bass: 3, treble: 0, loudness: true)))
}
private func listeningOrderCommand(_ desired: QueuePlaybackMode = .shuffleNoRepeat) throws -> NativeCommand {
    .listeningSettings(.order(try PlaybackOrderChange(group: listeningGroup, before: listeningOrder, desired: desired)))
}
private func listeningWait(_ test: () async -> Bool) async throws {
    for _ in 0..<2_000 {
        if await test() { return }
        try await Task.sleep(for: .milliseconds(1))
    }
    throw ContractError.invalid("Test request did not reach its hold point")
}

@Suite("Native listening settings command integration") struct ListeningSettingsCommandTests {
    @Test func actualClientReadsQualifiedMemberAndCoordinatorRoutesOnly() async throws {
        let transport = ListeningDaemon(), client = try listeningClient(transport)
        let tone = try await client.listeningSettings(.speaker(listeningGroup, room: "Kitchen"))
        let mode = try await client.listeningSettings(.order(listeningGroup))
        #expect(tone == .speaker(listeningTone)); #expect(mode == .order(listeningOrder))
        #expect(await transport.calls.map(\.path) == ["/zones/Kitchen@HOUSE_A/eq", "/zones/Studio@HOUSE_A/play-mode"])
        #expect(await transport.writes.isEmpty)
    }
    @Test func invalidMemberCannotDispatchEvenARead() async throws {
        let transport = ListeningDaemon(), client = try listeningClient(transport)
        await #expect(throws: ContractError.self) {
            try await client.speakerTone(RoomID(household: "HOUSE_B", room: "Kitchen"), in: listeningGroup)
        }
        #expect(await transport.calls.isEmpty)
    }
    @Test func toneUsesSharedReviewAndExactlyOneMemberWrite() async throws {
        let transport = ListeningDaemon(), session = CommandReviewSession(client: try listeningClient(transport))
        let command = try listeningToneCommand(), review = try await session.prepare(command, in: listeningGroup)
        #expect(await transport.writes.isEmpty)
        #expect(review.command.room == "Kitchen")
        #expect(review.command.explanation.contains("Kitchen@HOUSE_A"))
        let receipt = try await session.execute(review)
        #expect(!receipt.partial); #expect(receipt.title == "Speaker sound updated")
        let writes = await transport.writes
        #expect(writes.count == 1); #expect(writes.first?.path == "/eq")
        #expect(writes.first?.body == ["zone": .string("Kitchen@HOUSE_A"), "bass": .integer(3)])
        #expect(await transport.calls.filter { $0.path.hasSuffix("/eq") && $0.method == "GET" }.count == 2)
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes.count == 1)
    }
    @Test func allModesTravelThroughTheProductionSingleShotClient() async throws {
        for mode in QueuePlaybackMode.allCases {
            let transport = ListeningDaemon(), client = try listeningClient(transport)
            let before: QueuePlaybackMode = mode == .normal ? .repeatAll : .normal
            await transport.setOrder(before)
            let command = NativeCommand.listeningSettings(.order(try PlaybackOrderChange(group: listeningGroup,
                before: QueuePlaybackOrder(zone: "Studio", mode: before, queueActive: true, djActive: false), desired: mode)))
            let session = CommandReviewSession(client: client)
            _ = try await session.execute(session.prepare(command, in: listeningGroup))
            let write = try #require(await transport.writes.first)
            #expect(write.path == "/play-mode")
            #expect(write.body == ["zone": .string("Studio@HOUSE_A"), "mode": .string(mode.rawValue), "expected_mode": .string(before.rawValue)])
            #expect(await transport.writes.count == 1)
        }
    }
    @Test func naturalTrackAdvanceDoesNotTurnAnOrderChangeIntoPlayback() async throws {
        let transport = ListeningDaemon(), session = CommandReviewSession(client: try listeningClient(transport))
        let review = try await session.prepare(listeningOrderCommand(), in: listeningGroup)
        await transport.advanceTrack()
        _ = try await session.execute(review)
        #expect(await transport.writes.map(\.path) == ["/play-mode"])
    }
    @Test func editedAndUneditedToneConflictsStopBeforeDispatch() async throws {
        for changedBass in [true, false] {
            let transport = ListeningDaemon(), session = CommandReviewSession(client: try listeningClient(transport))
            let review = try await session.prepare(listeningToneCommand(), in: listeningGroup)
            await transport.setTone(bass: changedBass ? 1 : 0, treble: changedBass ? 0 : 1)
            await #expect(throws: ContractError.self) { try await session.execute(review) }
            #expect(await transport.writes.isEmpty)
        }
    }
    @Test func sourceDJAndModeAreRecheckedAtConfirmation() async throws {
        for change in 0..<3 {
            let transport = ListeningDaemon(), session = CommandReviewSession(client: try listeningClient(transport))
            let review = try await session.prepare(listeningOrderCommand(), in: listeningGroup)
            await transport.setOrder(change == 0 ? .repeatAll : .normal, queue: change != 1, dj: change == 2)
            await #expect(throws: ContractError.self) { try await session.execute(review) }
            #expect(await transport.writes.isEmpty)
        }
    }
    @Test func regroupedOrOtherHouseholdDestinationCannotReceiveTheCommand() async throws {
        for home in ["HOUSE_A", "HOUSE_B"] {
            let transport = ListeningDaemon(), session = CommandReviewSession(client: try listeningClient(transport))
            let review = try await session.prepare(listeningToneCommand(), in: listeningGroup)
            await transport.setGroup(Zone(household: home, coordinatorRoom: "Studio", members: ["Studio"], transportState: "paused"))
            await #expect(throws: ContractError.self) { try await session.execute(review) }
            #expect(await transport.writes.isEmpty)
        }
    }
    @Test func failedSettingsReadIsNotPermissionToWrite() async throws {
        let transport = ListeningDaemon(), session = CommandReviewSession(client: try listeningClient(transport))
        await transport.fail("/zones/Kitchen@HOUSE_A/eq")
        await #expect(throws: (any Error).self) { try await session.prepare(listeningToneCommand(), in: listeningGroup) }
        #expect(await transport.writes.isEmpty)
    }
    @Test func cancellingAFinalHeldReadCannotSendTheSetting() async throws {
        let transport = ListeningDaemon(), session = CommandReviewSession(client: try listeningClient(transport))
        let review = try await session.prepare(listeningOrderCommand(), in: listeningGroup)
        await transport.hold("/zones/Studio@HOUSE_A/play-mode")
        let work = Task { try await session.execute(review) }
        try await listeningWait { await transport.waiting != nil }
        await session.cancel(); await transport.release()
        await #expect(throws: CancellationError.self) { try await work.value }
        #expect(await transport.writes.isEmpty)
    }
    @Test func slowSettingSharesThePlaybackGateAndConsumesItsReview() async throws {
        let transport = ListeningDaemon(), session = CommandReviewSession(client: try listeningClient(transport))
        let review = try await session.prepare(listeningToneCommand(), in: listeningGroup)
        await transport.hold("/eq", method: "POST")
        let work = Task { try await session.execute(review) }
        try await listeningWait { await transport.waiting != nil }
        await #expect(throws: ContractError.self) { try await session.prepare(.next, in: listeningGroup) }
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        await transport.release(); _ = try await work.value
        #expect(await transport.writes.count == 1)
    }
    @Test func lostAndMalformedSettingsRepliesNeverReplay() async throws {
        for tone in [true, false] {
            for malformed in [true, false] {
                let transport = ListeningDaemon(), session = CommandReviewSession(client: try listeningClient(transport))
                let review = try await session.prepare(tone ? listeningToneCommand() : listeningOrderCommand(), in: listeningGroup)
                if malformed { await transport.malformed() } else { await transport.fail(tone ? "/eq" : "/play-mode") }
                await #expect(throws: ClientError.self) { try await session.execute(review) }
                await #expect(throws: ContractError.self) { try await session.execute(review) }
                #expect(await transport.writes.count == 1)
            }
        }
    }
    @Test func recoveredReceiptDoesNotClaimThisClientCausedTheSetting() async throws {
        let transport = ListeningDaemon(), session = CommandReviewSession(client: try listeningClient(transport))
        await transport.recover()
        let review = try await session.prepare(listeningOrderCommand(), in: listeningGroup)
        let receipt = try await session.execute(review)
        #expect(receipt.title == "Playback order observed"); #expect(receipt.detail.contains("does not prove"))
    }
    @Test func newerPlaybackReviewRetiresAnUnsentSoundReview() async throws {
        let transport = ListeningDaemon(), session = CommandReviewSession(client: try listeningClient(transport))
        let old = try await session.prepare(listeningToneCommand(), in: listeningGroup)
        let fresh = try await session.prepare(.pause, in: listeningGroup)
        await #expect(throws: ContractError.self) { try await session.execute(old) }
        _ = try await session.execute(fresh)
        #expect(await transport.writes.map(\.path) == ["/pause"])
    }
}
