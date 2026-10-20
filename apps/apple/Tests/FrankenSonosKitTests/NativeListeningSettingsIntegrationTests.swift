import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

private func settingsIntegrationGroup(_ household: String = "TEST-A") -> Zone {
    Zone(household: household, coordinatorRoom: "Studio", members: ["Studio", "Kitchen"], transportState: "paused")
}
private func settingsIntegrationTone(_ room: String = "Kitchen", bass: Int = 0, treble: Int = 0, loudness: Bool = true) -> SpeakerTone {
    SpeakerTone(room: room, bass: bass, treble: treble, loudness: loudness)
}

/// Reads and writes traverse the real settings draft, daemon client, remote
/// model and command actor. This daemon applies a write before delaying or
/// losing its acknowledgement, so a refresh cannot substitute for that ACK.
actor SettingsIntegrationTransport: DaemonTransport {
    enum Reply { case normal, lost, cancelled, malformed, partialTone }
    var requests: [URLRequest] = []
    var tones: [RoomID: SpeakerTone] = [:]
    var modes: [RoomID: QueuePlaybackMode] = [:]
    var scenes: [Scene] = []
    var reply: Reply = .normal
    var applied = 0
    var blocked = false
    private var heldKey: String?
    private var held: CheckedContinuation<Void, Never>?
    private let groups = [settingsIntegrationGroup(), settingsIntegrationGroup("TEST-B")]

    func respond(_ value: Reply) { reply = value }
    func hold(_ method: String, _ path: String) { heldKey = method + " " + path }
    func release() { let continuation = held; held = nil; blocked = false; continuation?.resume() }
    func writes() -> [URLRequest] { requests.filter { $0.httpMethod == "POST" } }
    func mutations() -> [URLRequest] { requests.filter { ($0.httpMethod ?? "GET") != "GET" } }

    private func tone(_ room: RoomID) -> SpeakerTone { tones[room] ?? settingsIntegrationTone(room.room) }
    private func toneJSON(_ room: RoomID) -> JSONValue {
        let value = tone(room)
        return .object(["room": .string(value.room), "bass": .integer(Int64(value.bass)),
                        "treble": .integer(Int64(value.treble)), "loudness": .bool(value.loudness)])
    }
    private func orderJSON(_ group: Zone) -> JSONValue {
        .object(["zone": .string(group.coordinatorRoom), "mode": .string((modes[group.id] ?? .normal).rawValue),
                 "queue_active": .bool(true), "dj_active": .bool(false)])
    }
    private func json<T: Encodable>(_ value: T) throws -> HTTPReply {
        let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
        return HTTPReply(data: try encoder.encode(value), status: 200)
    }
    func send(_ request: URLRequest) async throws -> HTTPReply {
        requests.append(request)
        let method = request.httpMethod ?? "GET", path = request.url!.path
        let response: HTTPReply
        if method == "GET" {
            if path == "/zones" { response = try json(groups) }
            else if path == "/scenes" { response = try json(scenes) }
            else if path.hasSuffix("/state"), let group = groups.first(where: { group in
                group.rooms.contains { path == "/zones/" + $0.address + "/state" }
            }) {
                response = try json(ZoneState(zone: group, transportState: "paused", volume: 20, mute: false, groupVolume: 20,
                    track: Track(uri: "synthetic:track", title: "Fixture track", creator: nil, album: nil,
                                 durationSecs: 240, positionSecs: 30, queuePosition: 1), shuffle: false, repeat: "off"))
            } else if path.hasSuffix("/eq"), let room = groups.flatMap({ Array($0.rooms) }).first(where: {
                path == "/zones/" + $0.address + "/eq"
            }) { response = try json(toneJSON(room)) }
            else if path.hasSuffix("/play-mode"), let group = groups.first(where: {
                path == "/zones/" + $0.id.address + "/play-mode"
            }) { response = try json(orderJSON(group)) }
            else { throw ContractError.invalid("Unexpected settings integration read: " + path) }
        } else if method == "POST" {
            let body = try JSONDecoder().decode([String: JSONValue].self, from: request.httpBody ?? Data())
            guard case .string(let target) = body["zone"] else { throw ContractError.invalid("Missing settings target") }
            if path == "/eq", let room = groups.flatMap({ Array($0.rooms) }).first(where: { $0.address == target }) {
                let old = tone(room)
                let bass = if case .integer(let value) = body["bass"] { Int(value) } else { old.bass }
                let treble = if case .integer(let value) = body["treble"], reply != .partialTone { Int(value) } else { old.treble }
                let loudness = if case .bool(let value) = body["loudness"], reply != .partialTone { value } else { old.loudness }
                tones[room] = settingsIntegrationTone(room.room, bass: bass, treble: treble, loudness: loudness)
                response = try json(["done": JSONValue.string("Tone applied"), "eq": toneJSON(room)])
            } else if path == "/play-mode", let group = groups.first(where: { $0.id.address == target }),
                      case .string(let expected) = body["expected_mode"], expected == (modes[group.id] ?? .normal).rawValue,
                      case .string(let raw) = body["mode"], let mode = QueuePlaybackMode(rawValue: raw) {
                modes[group.id] = mode
                response = try json(["done": JSONValue.string("Order applied"), "state": orderJSON(group),
                                     "changed": .bool(true), "recovered": .bool(false)])
            } else { throw ContractError.invalid("Unexpected settings integration write: " + path) }
            applied += 1
        } else if method == "PUT", path.hasPrefix("/scenes/") {
            let name = String(path.dropFirst("/scenes/".count))
            let scene = Scene(name: name, saved: 1,
                groups: [.init(coordinator: "Studio@TEST-A", members: ["Kitchen@TEST-A"],
                               source: .init(kind: "keep", name: nil, uri: nil, mood: nil), playing: nil)],
                volumes: ["Studio@TEST-A": 20], mutes: [:])
            scenes.append(scene); applied += 1
            response = try json(scene)
        } else { throw ContractError.invalid("Unexpected settings request method") }
        if heldKey == method + " " + path {
            heldKey = nil; blocked = true
            await withCheckedContinuation { held = $0 }
        }
        if method == "POST" {
            switch reply {
            case .lost: throw URLError(.networkConnectionLost)
            case .cancelled: throw CancellationError()
            case .malformed: return HTTPReply(data: Data("{}".utf8), status: 200)
            case .normal, .partialTone: break
            }
        }
        return response
    }
    nonisolated func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> {
        AsyncThrowingStream { $0.yield(.connected) }
    }
}

@MainActor private func settingsIntegrationWait(_ condition: () -> Bool) async throws {
    let deadline = ContinuousClock.now.advanced(by: .seconds(3))
    while !condition() {
        guard ContinuousClock.now < deadline else { throw ContractError.invalid("Settings integration screen did not settle") }
        try await Task.sleep(for: .milliseconds(5))
    }
}
private func settingsIntegrationGate(_ transport: SettingsIntegrationTransport) async throws {
    let deadline = ContinuousClock.now.advanced(by: .seconds(3))
    while !(await transport.blocked) {
        guard ContinuousClock.now < deadline else { throw ContractError.invalid("Settings integration gate was not reached") }
        try await Task.sleep(for: .milliseconds(3))
    }
}
@MainActor private func settingsIntegrationScreen(_ transport: SettingsIntegrationTransport, tone: Bool) async throws -> (RemoteModel, ListeningSettingsModel) {
    let client = DaemonClient(endpoint: try DaemonEndpoint("https://music.example.test"), transport: transport)
    let remote = RemoteModel(client: client)
    remote.show(.scenes); remote.resume()
    try await settingsIntegrationWait { remote.canPrepare && !remote.panelLoading }
    let settings = ListeningSettingsModel(read: { try await client.listeningSettings($0) })
    let group = try #require(remote.selectedZone)
    settings.activate(tone ? .speaker(group, room: "Kitchen") : .order(group))
    try await settingsIntegrationWait { settings.isFresh && !settings.loading }
    if tone { settings.editTone(bass: 3) }
    else { settings.editOrder(shuffled: true, repetition: .queue) }
    return (remote, settings)
}
@MainActor private func settingsIntegrationReview(_ remote: RemoteModel, _ settings: ListeningSettingsModel) async throws -> CommandReview {
    let selection = try #require(settings.selection())
    remote.prepare(try settings.resolve(selection).command, in: selection.scope.group)
    try await settingsIntegrationWait { remote.review != nil && remote.actionPhase == .idle }
    return try #require(remote.review)
}

@Suite("Native listening controls and remote lifecycle") @MainActor struct NativeListeningSettingsIntegrationTests {
    @Test func realDraftSelectionSendsOneSettingAndKeepsTheMemberOrCoordinatorTarget() async throws {
        for tone in [true, false] {
            let transport = SettingsIntegrationTransport()
            let (remote, settings) = try await settingsIntegrationScreen(transport, tone: tone)
            defer { settings.stop(); remote.stop() }
            #expect(await transport.writes().isEmpty)
            let review = try await settingsIntegrationReview(remote, settings)
            #expect(remote.selectedRoom == settingsIntegrationGroup().id)
            remote.confirm(review.id); remote.confirm(review.id)
            try await settingsIntegrationWait { remote.result != nil && remote.actionPhase == .idle }
            let target = RoomID(household: "TEST-A", room: tone ? "Kitchen" : "Studio")
            #expect(remote.result?.kind == .confirmed && remote.result?.target == target)
            let writes = await transport.writes()
            #expect(writes.count == 1)
            let request = try #require(writes.first)
            #expect(request.url?.path == (tone ? "/eq" : "/play-mode"))
            let body = try JSONDecoder().decode([String: JSONValue].self, from: #require(request.httpBody))
            #expect(body == (tone ? ["zone": .string(target.address), "bass": .integer(3)] :
                ["zone": .string(target.address), "mode": .string("SHUFFLE"), "expected_mode": .string("NORMAL")]))
            settings.refresh()
            try await settingsIntegrationWait { settings.isFresh && !settings.loading }
            #expect(!settings.hasEdits && settings.selection() == nil)
            #expect(remote.result?.kind == .confirmed)
            #expect(await transport.applied == 1)
        }
    }

    @Test func capturedPanelSelectionCannotMoveToAnotherMemberOrSameNamedHousehold() async throws {
        for otherHousehold in [false, true] {
            let transport = SettingsIntegrationTransport()
            let (remote, settings) = try await settingsIntegrationScreen(transport, tone: true)
            defer { settings.stop(); remote.stop() }
            let selection = try #require(settings.selection())
            let oldCommand = try settings.resolve(selection).command
            let group = settingsIntegrationGroup(otherHousehold ? "TEST-B" : "TEST-A")
            settings.activate(.speaker(group, room: otherHousehold ? "Kitchen" : "Studio"))
            #expect(throws: ContractError.self) { try settings.resolve(selection) }
            if otherHousehold {
                remote.select(group.id)
                remote.prepare(oldCommand, in: selection.scope.group)
                #expect(remote.review == nil && remote.actionPhase == .idle)
            }
            try await settingsIntegrationWait { settings.isFresh && !settings.loading }
            #expect(settings.selection() == nil && !settings.hasEdits)
            #expect(await transport.writes().isEmpty)
        }
    }

    @Test func cancellingPreparationKeepsTheSharedSlotUntilItsReadDrains() async throws {
        let transport = SettingsIntegrationTransport()
        let (remote, settings) = try await settingsIntegrationScreen(transport, tone: true)
        defer { settings.stop(); remote.stop() }
        let selection = try #require(settings.selection())
        await transport.hold("GET", "/zones/Kitchen@TEST-A/eq")
        remote.prepare(try settings.resolve(selection).command, in: selection.scope.group)
        try await settingsIntegrationGate(transport)
        remote.cancelReview(); remote.prepare(.pause); remote.prepareUndo()
        #expect(remote.actionPhase == .preparing && remote.review == nil && remote.undoReview == nil)
        await transport.release()
        try await settingsIntegrationWait { remote.actionPhase == .idle }
        #expect(remote.review == nil && remote.result == nil)
        #expect(await transport.writes().isEmpty)
    }

    @Test func suspensionDuringTheFinalReadCannotSendEitherSetting() async throws {
        for tone in [true, false] {
            let transport = SettingsIntegrationTransport()
            let (remote, settings) = try await settingsIntegrationScreen(transport, tone: tone)
            defer { settings.stop(); remote.stop() }
            let review = try await settingsIntegrationReview(remote, settings)
            await transport.hold("GET", tone ? "/zones/Kitchen@TEST-A/eq" : "/zones/Studio@TEST-A/play-mode")
            remote.confirm(review.id)
            try await settingsIntegrationGate(transport)
            settings.suspend(); remote.suspend()
            await transport.release()
            try await settingsIntegrationWait { remote.result != nil && remote.actionPhase == .idle }
            #expect(remote.result?.kind == .notSent)
            #expect(remote.result?.target == RoomID(household: "TEST-A", room: tone ? "Kitchen" : "Studio"))
            remote.confirm(review.id)
            #expect(remote.review == nil)
            #expect(await transport.writes().isEmpty)
        }
    }

    @Test func lateAcknowledgementsKeepTheirOriginalMemberAfterNavigationOrSuspension() async throws {
        for tone in [true, false] {
            for suspend in [true, false] {
                let transport = SettingsIntegrationTransport()
                let (remote, settings) = try await settingsIntegrationScreen(transport, tone: tone)
                defer { settings.stop(); remote.stop() }
                let review = try await settingsIntegrationReview(remote, settings)
                await transport.hold("POST", tone ? "/eq" : "/play-mode")
                remote.confirm(review.id)
                try await settingsIntegrationGate(transport)
                if suspend { settings.suspend(); remote.suspend() }
                else {
                    remote.select(settingsIntegrationGroup("TEST-B").id)
                    settings.activate(tone ? .speaker(settingsIntegrationGroup("TEST-B"), room: "Kitchen") : .order(settingsIntegrationGroup("TEST-B")))
                }
                remote.confirm(review.id)
                await transport.release()
                try await settingsIntegrationWait { remote.result != nil && remote.actionPhase == .idle }
                #expect(remote.result?.kind == .confirmed)
                #expect(remote.result?.target == RoomID(household: "TEST-A", room: tone ? "Kitchen" : "Studio"))
                if !suspend { #expect(remote.selectedRoom?.household == "TEST-B") }
                #expect(await transport.applied == 1)
                #expect(await transport.writes().count == 1)
            }
        }
    }

    @Test func unknownOutcomeSurvivesActualReadBackAndNeverReplays() async throws {
        for tone in [true, false] {
            for reply in [SettingsIntegrationTransport.Reply.lost, .cancelled, .malformed] {
                let transport = SettingsIntegrationTransport()
                let (remote, settings) = try await settingsIntegrationScreen(transport, tone: tone)
                defer { settings.stop(); remote.stop() }
                let scope = try #require(settings.scope)
                let review = try await settingsIntegrationReview(remote, settings)
                await transport.respond(reply)
                remote.confirm(review.id)
                try await settingsIntegrationWait { remote.result?.kind == .uncertain && remote.actionPhase == .idle }
                remote.confirm(review.id); remote.refresh()
                settings.suspend(); remote.suspend(); remote.resume(); settings.activate(scope)
                try await settingsIntegrationWait { settings.isFresh && !settings.loading && remote.canPrepare }
                #expect(!settings.hasEdits && settings.selection() == nil)
                #expect(remote.result?.kind == .uncertain)
                #expect(remote.result?.target == RoomID(household: "TEST-A", room: tone ? "Kitchen" : "Studio"))
                #expect(remote.review == nil)
                #expect(await transport.applied == 1)
                #expect(await transport.writes().count == 1)
            }
        }
    }

    @Test func partiallyAppliedToneIsUncertainAndReadBackDoesNotClaimFullSuccess() async throws {
        let transport = SettingsIntegrationTransport()
        let (remote, settings) = try await settingsIntegrationScreen(transport, tone: true)
        defer { settings.stop(); remote.stop() }
        settings.editTone(treble: -2, loudness: false)
        let review = try await settingsIntegrationReview(remote, settings)
        await transport.respond(.partialTone)
        remote.confirm(review.id)
        try await settingsIntegrationWait { remote.result?.kind == .uncertain && remote.actionPhase == .idle }
        settings.refresh()
        try await settingsIntegrationWait { settings.isFresh && !settings.loading }
        #expect(settings.tone?.bass == 3 && settings.tone?.treble == 0 && settings.tone?.loudness == true)
        #expect(!settings.hasEdits && settings.notice != nil)
        #expect(remote.result?.kind == .uncertain)
        #expect(remote.result?.target == RoomID(household: "TEST-A", room: "Kitchen"))
        remote.confirm(review.id)
        #expect(await transport.writes().count == 1)
    }
}
