import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

private func nativeSceneSourceZone(_ household: String = "S2") -> Zone {
    Zone(household: household, coordinatorRoom: "Source room", members: ["Source room"], transportState: "playing")
}
private func nativeSceneTargetZone(_ household: String = "S2") -> Zone {
    Zone(household: household, coordinatorRoom: "Listening room", members: ["Listening room", "Hall"], transportState: "paused")
}
private func nativeSceneOtherZone(_ household: String = "S2") -> Zone {
    Zone(household: household, coordinatorRoom: "Study", members: ["Study"], transportState: "stopped")
}
private let nativeScenePlaylist = SonosPlaylist(id: "SQ:7", title: "Mixed native selection", artUri: nil)
private func nativeSceneURI(_ position: UInt32) -> String {
    position == 2 ? "x-file-cifs://library.example.test/Cafe\u{301}.flac"
        : "x-sonos-service:fixture%3atrack%3a\(position)?account=synthetic"
}
private func nativeSceneTitle(_ position: UInt32) -> String {
    position == 2 ? "Cafe\u{301}\nFirst movement" : "Selected track \(position)"
}
private func nativeScenePage(_ offset: UInt32 = 0, revision: String = "native:v1", changed: Bool = false) throws -> Data {
    let count = min(UInt32(50), 60 - offset)
    let items = (0..<count).map { index -> JSONValue in
        let position = offset + index + 1
        return .object([
            "position": .integer(Int64(position)), "title": .string(nativeSceneTitle(position)),
            "uri": .string(changed && position == 51 ? "fixture:changed" : nativeSceneURI(position))
        ])
    }
    return try JSONEncoder().encode([
        "zone": JSONValue.string("Source room"),
        "playlist": .object(["id": .string(nativeScenePlaylist.id), "title": .string(nativeScenePlaylist.title)]),
        "revision": .string(revision), "total": .integer(60), "offset": .integer(Int64(offset)),
        "next_offset": offset == 0 ? .integer(50) : .null, "items": .array(items)
    ])
}
private func nativeSceneSelection(_ household: String = "S2", target: String? = nil) throws -> SonosPlaylistTracksSelection {
    var draft = SonosPlaylistTracksDraft()
    for (offset, index) in [(UInt32(0), 1), (UInt32(50), 0), (UInt32(0), 1)] {
        let request = try SonosPlaylistContentsRequest(playlist: nativeScenePlaylist, in: nativeSceneSourceZone(household),
            offset: offset, expectedRevision: offset == 0 ? nil : "native:v1")
        let page = try DaemonClient.decode(SonosPlaylistContents.self, from: nativeScenePage(offset))
        try draft.append(page.items[index], from: page, request: request)
    }
    return try SonosPlaylistTracksSelection(draft, action: .play, target: target)
}
private func nativeSceneDefinition(_ household: String = "S2", playing: Bool = true) throws -> SceneDefinition {
    var draft = try SceneDraft(nativeSelection: nativeSceneSelection(household), destinations: [nativeSceneTargetZone(household)])
    draft.name = "Native evening"
    draft.groups[0].playback = playing ? .playing : .paused
    return try draft.definition(zones: [nativeSceneSourceZone(household), nativeSceneTargetZone(household)])
}
private func nativeSceneJSON<T: Encodable>(_ value: T, status: Int = 200) throws -> HTTPReply {
    let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
    return HTTPReply(data: try encoder.encode(value), status: status)
}

/// Only scene storage is writable. Production review, source validation,
/// serialization, receipts and lifecycle run over this transport fixture.
private actor NativeSceneTransport: DaemonTransport {
    let household: String
    var requests: [URLRequest] = []
    var pages: [UInt32] = []
    var rows: [Scene] = []
    var revision = "native:v1"
    var changedTrack = false
    var sourceRegrouped = false
    var destinationRegrouped = false
    var loseReply = false
    var corruptReply: String?
    var conflict = false
    var heldOffset: UInt32?
    var blocked = false
    var held: CheckedContinuation<Void, Never>?
    nonisolated let stream: AsyncThrowingStream<EventSignal, Error>
    nonisolated let output: AsyncThrowingStream<EventSignal, Error>.Continuation

    init(_ household: String = "S2") {
        self.household = household
        let pair = AsyncThrowingStream<EventSignal, Error>.makeStream()
        stream = pair.stream; output = pair.continuation
    }
    var writes: [URLRequest] { requests.filter { ($0.httpMethod ?? "GET") != "GET" } }
    func change(_ field: String) {
        switch field {
        case "revision": revision = "native:v2"
        case "occurrence": changedTrack = true
        case "source": sourceRegrouped = true
        case "destination": destinationRegrouped = true
        default: break
        }
    }
    func failReply(_ field: String) { if field == "lost" { loseReply = true } else { corruptReply = field } }
    func failCAS() { conflict = true }
    func hold(_ offset: UInt32) { heldOffset = offset }
    func release() { heldOffset = nil; blocked = false; held?.resume(); held = nil }

    func send(_ request: URLRequest) async throws -> HTTPReply {
        requests.append(request)
        let path = request.url!.path.removingPercentEncoding ?? request.url!.path
        let query = URLComponents(url: request.url!, resolvingAgainstBaseURL: false)?.queryItems ?? []
        let source = nativeSceneSourceZone(household), target = nativeSceneTargetZone(household), other = nativeSceneOtherZone(household)
        if request.httpMethod == "PUT" {
            #expect(path == "/scenes/Native evening")
            let fields = try JSONDecoder().decode([String: JSONValue].self, from: #require(request.httpBody))
            #expect(fields["expected_revision"] == .string("absent"))
            if conflict {
                return try nativeSceneJSON(APIProblem(code: "CONFIG_CHANGED", detail: "Another author saved this name.", hint: nil), status: 409)
            }
            guard case .array(let requestedGroups)? = fields["groups"] else { throw ClientError.unreadable }
            var savedGroups: [JSONValue] = []
            for requested in requestedGroups {
                guard case .object(var group) = requested,
                      case .object(let requestedSource)? = group["source"],
                      case .object(let selected)? = requestedSource["selection"],
                      case .array(let positions)? = selected["positions"] else { throw ClientError.unreadable }
                #expect(Set(requestedSource.keys) == ["kind", "name", "selection"])
                #expect(selected["zone"] == .string(source.id.address))
                #expect(selected["expected_revision"] == .string("native:v1"))
                #expect(Set(selected.keys) == ["zone", "positions", "expected_revision"])
                var tracks = try positions.map { position -> JSONValue in
                    guard case .integer(let raw) = position, let position = UInt32(exactly: raw) else { throw ClientError.unreadable }
                    return .object(["source_uri": .string(nativeSceneURI(position)), "title": .string(nativeSceneTitle(position))])
                }
                if corruptReply == "order", tracks.count > 1 { tracks.swapAt(0, 1) }
                if corruptReply == "resource", case .object(var track) = tracks[0] {
                    track["source_uri"] = .string("x-file-cifs://library.example.test/Caf\u{e9}.flac")
                    tracks[0] = .object(track)
                }
                group["source"] = .object([
                    "kind": .string("sonos_playlist_tracks"), "name": .string(nativeScenePlaylist.id),
                    "playlist": .object([
                        "household": .string(corruptReply == "household" ? "OTHER" : household),
                        "id": .string(nativeScenePlaylist.id), "title": .string(nativeScenePlaylist.title)
                    ]), "tracks": .array(tracks)
                ])
                savedGroups.append(.object(group))
            }
            let reply: [String: JSONValue] = [
                "name": .string("Native evening"), "saved": .integer(1_800_000_000),
                "revision": .string("scene:created"), "groups": .array(savedGroups),
                "volumes": fields["volumes"] ?? .object([:]), "mutes": fields["mutes"] ?? .object([:])
            ]
            let data = try JSONEncoder().encode(reply)
            rows = [try DaemonClient.decode(Scene.self, from: data)]
            if loseReply { throw URLError(.networkConnectionLost) }
            return HTTPReply(data: data, status: 200)
        }
        guard (request.httpMethod ?? "GET") == "GET" else {
            throw ContractError.invalid("Scene creation attempted a playback write: \(path)")
        }
        switch path {
        case "/zones":
            let currentSource = sourceRegrouped ? Zone(household: household, coordinatorRoom: "Source room", members: ["Source room", "New member"], transportState: "playing") : source
            let currentTarget = destinationRegrouped ? Zone(household: household, coordinatorRoom: "Hall", members: target.members, transportState: "paused") : target
            return try nativeSceneJSON([currentSource, currentTarget, other])
        case "/scenes": return try nativeSceneJSON(rows)
        case "/sonos/playlists": return try nativeSceneJSON([nativeScenePlaylist])
        case "/favorites": return try nativeSceneJSON([Favorite]())
        case "/sonos/playlists/contents":
            #expect(query.contains(.init(name: "zone", value: source.id.address)))
            let offset = UInt32(query.first(where: { $0.name == "offset" })?.value ?? "")!
            pages.append(offset)
            if heldOffset == offset {
                heldOffset = nil; blocked = true
                await withCheckedContinuation { held = $0 }
            }
            return HTTPReply(data: try nativeScenePage(offset, revision: revision, changed: changedTrack), status: 200)
        case "/rooms/resolve":
            #expect(query.contains(.init(name: "target", value: "downstairs")))
            func member(_ room: String, coordinator: String) -> JSONValue {
                .object(["room": .string(room), "household": .string(household),
                         "player": .string("FIXTURE_" + room), "coordinator": .string(coordinator)])
            }
            let first = member("Listening room", coordinator: "FIXTURE_1")
            let second = member("Study", coordinator: "FIXTURE_2")
            return try nativeSceneJSON([
                "target": JSONValue.string("downstairs"), "matched": .string("alias"),
                "revision": .string(String(repeating: "a", count: 64)), "rooms": .array([first, second]),
                "groups": .array([
                    .object(["household": .string(household), "coordinator": .string("FIXTURE_1"),
                        "coordinator_room": .string("Listening room"), "members": .array([first, member("Hall", coordinator: "FIXTURE_1")])]),
                    .object(["household": .string(household), "coordinator": .string("FIXTURE_2"),
                        "coordinator_room": .string("Study"), "members": .array([second])])
                ])
            ])
        default:
            if path.hasPrefix("/scenes/"), let row = rows.first { return try nativeSceneJSON(row) }
            if path.hasSuffix("/state"), let zone = [source, target, other].first(where: { path == "/zones/" + $0.id.address + "/state" }) {
                return try nativeSceneJSON(ZoneState(zone: zone, transportState: "playing", volume: 20,
                    mute: false, groupVolume: 20, track: nil, shuffle: false, repeat: "off"))
            }
            if path.hasSuffix("/queue") {
                return try nativeSceneJSON(QueuePage(total: 0, items: [], playing: nil, revision: "native-scene-empty"))
            }
            throw ContractError.invalid("Unexpected scene read: \(path)")
        }
    }
    nonisolated func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> { stream }
}

private func nativeSceneClient(_ transport: NativeSceneTransport) throws -> DaemonClient {
    DaemonClient(endpoint: try DaemonEndpoint("https://music.example.test"), transport: transport)
}
@MainActor private func nativeSceneWait(_ condition: () -> Bool) async throws {
    let deadline = ContinuousClock.now.advanced(by: .seconds(3))
    while !condition() {
        guard ContinuousClock.now < deadline else { throw ContractError.invalid("Native scene model condition was not reached.") }
        try await Task.sleep(for: .milliseconds(3))
    }
}
private func nativeSceneGate(_ transport: NativeSceneTransport) async throws {
    let deadline = ContinuousClock.now.advanced(by: .seconds(3))
    while !(await transport.blocked) {
        guard ContinuousClock.now < deadline else { throw ContractError.invalid("The native scene read gate was not reached.") }
        try await Task.sleep(for: .milliseconds(3))
    }
}

@Suite("Native selected-track scene authoring")
struct NativeTrackSceneTests {
    @Test(arguments: ["S1", "S2"], [true, false])
    func createsFrozenNativeScenesWithOneConditionalSave(household: String, playing: Bool) async throws {
        let transport = NativeSceneTransport(household), client = try nativeSceneClient(transport)
        let session = CommandReviewSession(client: client)
        let definition = try nativeSceneDefinition(household, playing: playing)
        let review = try await session.prepare(SceneMutation.save(name: "Native evening", definition: definition, replacing: nil))
        let preview = try #require(review.preview?.groups.first)
        #expect(preview.coordinator == nativeSceneTargetZone(household).id.address)
        #expect(preview.source.tracks?.map(\.sourceUri) == [nativeSceneURI(2), nativeSceneURI(51), nativeSceneURI(2)])
        #expect(preview.source.tracks?.first?.sourceUri.utf8.elementsEqual(nativeSceneURI(2).utf8) == true)
        #expect(preview.playing == playing)
        #expect(await transport.pages == [0, 50])
        #expect(await transport.writes.isEmpty)
        let receipt = try await session.execute(review)
        #expect(receipt.saved?.groups == review.preview?.groups)
        #expect(receipt.saved?.revision == "scene:created")
        #expect(await transport.pages == [0, 50, 0, 50])
        let writes = await transport.writes
        #expect(writes.count == 1 && writes.first?.httpMethod == "PUT")
        let encoded = try JSONDecoder().decode([String: JSONValue].self, from: JSONEncoder().encode(definition))
        #expect(encoded["groups"] == .array(try definition.requestGroups()))
        #expect(Set(encoded.keys) == ["groups", "volumes", "mutes"])
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes.count == 1)
    }

    @Test func roomSetsBecomeExplicitSceneGroupsWhileTheSourceStaysIndependent() async throws {
        let transport = NativeSceneTransport(), client = try nativeSceneClient(transport)
        let selection = try nativeSceneSelection(target: "downstairs")
        let targets = try await client.nativeSceneDestinations(selection)
        #expect(targets == [nativeSceneTargetZone(), nativeSceneOtherZone()])
        var draft = try SceneDraft(nativeSelection: selection, destinations: targets)
        draft.name = "Native evening"
        let definition = try draft.definition(zones: [nativeSceneSourceZone()] + targets)
        let session = CommandReviewSession(client: client)
        let review = try await session.prepare(SceneMutation.save(name: draft.name, definition: definition, replacing: nil))
        #expect(review.preview?.groups.map(\.coordinator) == targets.map(\.id.address))
        #expect(await transport.pages == [0, 50, 0, 50]) // Source pages are not repeated for each destination.
        _ = try await session.execute(review)
        #expect(await transport.writes.count == 1)
    }

    @Test(arguments: ["revision", "occurrence", "source", "destination"])
    func changedSourceOrDestinationInvalidatesTheReviewedSave(change: String) async throws {
        let transport = NativeSceneTransport(), session = CommandReviewSession(client: try nativeSceneClient(transport))
        let review = try await session.prepare(SceneMutation.save(name: "Native evening", definition: nativeSceneDefinition(), replacing: nil))
        await transport.change(change)
        await #expect(throws: (any Error).self) { try await session.execute(review) }
        #expect(await transport.writes.isEmpty)
    }

    @Test(arguments: ["source", "destination"])
    func regroupingDuringTheLastSourceReadPreventsTheSave(change: String) async throws {
        let transport = NativeSceneTransport(), session = CommandReviewSession(client: try nativeSceneClient(transport))
        let review = try await session.prepare(SceneMutation.save(name: "Native evening", definition: nativeSceneDefinition(), replacing: nil))
        await transport.hold(50)
        let confirmation = Task { try await session.execute(review) }
        try await nativeSceneGate(transport)
        await transport.change(change)
        await transport.release()
        await #expect(throws: ContractError.self) { try await confirmation.value }
        #expect(await transport.writes.isEmpty)
    }

    @Test func savedDisplayCannotBeForgedIntoAnAuthoredNativeSelectionOrAnotherHousehold() throws {
        let definition = try nativeSceneDefinition()
        let zones = [nativeSceneSourceZone(), nativeSceneTargetZone(), nativeSceneTargetZone("S1")]
        #expect(throws: ContractError.self) { try SceneDefinition(groups: definition.groups).validated(zones: zones) }
        var draft = try SceneDraft(nativeSelection: nativeSceneSelection(), destinations: [nativeSceneTargetZone()])
        draft.name = "Native evening"
        draft.groups[0].coordinator = nativeSceneTargetZone("S1").id
        draft.groups[0].members = []
        #expect(throws: ContractError.self) { try draft.definition(zones: zones) }
        let body = try JSONEncoder().encode(definition)
        let decoded = try DaemonClient.decode(SceneDefinition.self, from: body)
        #expect(throws: ContractError.self) { try decoded.validated(zones: zones) }
    }

    @Test(arguments: ["lost", "order", "resource", "household"])
    func lostOrContradictoryNativeSaveReceiptsAreUncertainAndNeverRetried(field: String) async throws {
        let transport = NativeSceneTransport(), session = CommandReviewSession(client: try nativeSceneClient(transport))
        let review = try await session.prepare(SceneMutation.save(name: "Native evening", definition: nativeSceneDefinition(), replacing: nil))
        await transport.failReply(field)
        do {
            _ = try await session.execute(review)
            Issue.record("A lost or changed native scene result was accepted.")
        } catch {
            if case ClientError.uncertain = error { } else { Issue.record("Expected uncertainty after a sent save: \(error)") }
        }
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes.count == 1)
    }

    @Test func aLateSceneNameConflictPreventsStorageAndPlayback() async throws {
        let transport = NativeSceneTransport(), session = CommandReviewSession(client: try nativeSceneClient(transport))
        let review = try await session.prepare(SceneMutation.save(name: "Native evening", definition: nativeSceneDefinition(), replacing: nil))
        await transport.failCAS()
        do {
            _ = try await session.execute(review)
            Issue.record("The conditional native scene save should have conflicted.")
        } catch let error as APIProblem { #expect(error.code == "CONFIG_CHANGED") }
        #expect(await transport.writes.count == 1)
        #expect(await transport.rows.isEmpty)
    }

    @Test func cancellingTheFinalNativePageReadPreventsTheSceneWrite() async throws {
        let transport = NativeSceneTransport(), session = CommandReviewSession(client: try nativeSceneClient(transport))
        let review = try await session.prepare(SceneMutation.save(name: "Native evening", definition: nativeSceneDefinition(), replacing: nil))
        await transport.hold(50)
        let confirmation = Task { try await session.execute(review) }
        try await nativeSceneGate(transport)
        await session.cancel()
        await transport.release()
        await #expect(throws: CancellationError.self) { try await confirmation.value }
        #expect(await transport.writes.isEmpty)
    }

    @MainActor @Test func theNativeSelectionScreenOpensAndSavesTheExactSceneDraft() async throws {
        let transport = NativeSceneTransport(), model = RemoteModel(client: try nativeSceneClient(transport))
        model.resume(); defer { model.stop() }
        try await nativeSceneWait { model.selectedZone != nil && !model.panelLoading }
        model.select(nativeSceneSourceZone().id)
        model.show(.sonosPlaylists)
        try await nativeSceneWait { !model.panelLoading && model.sonosPlaylists.count == 1 }
        let zone = try #require(model.selectedZone)
        model.openSonosPlaylist(nativeScenePlaylist, in: zone)
        try await nativeSceneWait { !model.panelLoading && model.sonosPlaylistContents != nil }
        let first = try #require(model.sonosPlaylistContents), request = try #require(model.sonosPlaylistRequest)
        model.addSonosPlaylistTrack(first.items[1], from: first, request: request, in: zone)
        let stale = model.sonosPlaylistTracks.id
        model.changeSonosPlaylistTrack(try #require(model.sonosPlaylistTracks.choices.first?.id), selection: stale, repeatEntry: true)
        model.beginNativeTrackScene(selection: stale, in: zone)
        #expect(model.panel == .sonosPlaylists && !model.sceneComposerOpen)
        model.changeSonosPlaylistDestination("downstairs", selection: model.sonosPlaylistTracks.id)
        model.beginNativeTrackScene(selection: model.sonosPlaylistTracks.id, in: zone)
        try await nativeSceneWait { model.canManageScenes && model.sceneComposerOpen }
        #expect(model.panel == .scenes && model.sceneDraft.groups.count == 2)
        #expect(model.sceneDraft.groups.allSatisfy { $0.nativeSelection?.positions == [2, 2] })
        #expect(await transport.writes.isEmpty)
        var draft = model.sceneDraft; draft.name = "Native evening"
        draft.groups[0].playback = .paused
        model.updateSceneDraft(draft); model.prepareSceneDraft()
        try await nativeSceneWait { model.sceneMutationReview != nil && model.actionPhase == .idle }
        let review = try #require(model.sceneMutationReview)
        model.confirmSceneMutation(review.id)
        try await nativeSceneWait { model.sceneMutationResult?.kind == .confirmed && model.actionPhase == .idle }
        #expect(!model.sceneComposerOpen)
        #expect(model.sceneMutationResult?.receipt?.saved?.groups.first?.playing == false)
        #expect(await transport.writes.count == 1)
    }

    @MainActor @Test func suspendingDuringSceneImportCannotPublishALateDraft() async throws {
        let transport = NativeSceneTransport(), model = RemoteModel(client: try nativeSceneClient(transport))
        model.resume(); defer { model.stop() }
        try await nativeSceneWait { model.selectedZone != nil && !model.panelLoading }
        model.select(nativeSceneSourceZone().id)
        model.show(.sonosPlaylists)
        try await nativeSceneWait { !model.panelLoading && model.sonosPlaylists.count == 1 }
        let zone = try #require(model.selectedZone)
        model.openSonosPlaylist(nativeScenePlaylist, in: zone)
        try await nativeSceneWait { !model.panelLoading && model.sonosPlaylistContents != nil }
        let page = try #require(model.sonosPlaylistContents), request = try #require(model.sonosPlaylistRequest)
        model.addSonosPlaylistTrack(page.items[1], from: page, request: request, in: zone)
        await transport.hold(0)
        model.beginNativeTrackScene(selection: model.sonosPlaylistTracks.id, in: zone)
        try await nativeSceneGate(transport)
        model.suspend()
        await transport.release()
        try await nativeSceneWait { model.actionPhase == .idle }
        #expect(!model.sceneComposerOpen && model.sceneDraft.groups.isEmpty)
        #expect(await transport.writes.isEmpty)
    }
}
