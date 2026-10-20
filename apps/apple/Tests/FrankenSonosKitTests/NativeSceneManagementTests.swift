import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

private let sceneManagementName = "Evening / café & friends"
private let sceneManagementRoom = RoomID(household: "TEST-A", room: "Studio")
private let sceneManagementOther = RoomID(household: "TEST-B", room: "Studio")
private func sceneManagementZone(_ household: String = "TEST-A") -> Zone {
    Zone(household: household, coordinatorRoom: "Studio", members: ["Studio", "Hall"], transportState: "playing")
}
private func sceneManagementSaved(_ name: String = sceneManagementName, level: Int = 24, at: Int64 = 1) -> Scene {
    Scene(name: name, saved: at,
        groups: [.init(coordinator: "Studio@TEST-A", members: ["Hall@TEST-A"],
            source: .init(kind: "keep", name: nil, uri: nil, mood: nil), playing: nil)],
        volumes: ["Hall@TEST-A": level], mutes: ["Studio@TEST-A": false])
}
private func sceneManagementCaptured(_ name: String) -> Scene {
    Scene(name: name, saved: 1_800_000_000,
        groups: [
            .init(coordinator: "Studio@TEST-A", members: ["Hall@TEST-A"],
                  source: .init(kind: "uri", name: nil, uri: "x-rincon-mp3radio:https://radio.example.test/live", mood: nil), playing: true),
            .init(coordinator: "Studio@TEST-B", members: ["Hall@TEST-B"],
                  source: .init(kind: "dj", name: nil, uri: nil, mood: "calm"), playing: false)
        ], volumes: ["Studio@TEST-A": 17, "Hall@TEST-B": 29],
        mutes: ["Studio@TEST-A": false, "Hall@TEST-B": true])
}
private func sceneManagementJSON<T: Encodable>(_ value: T, status: Int = 200) throws -> HTTPReply {
    let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
    return HTTPReply(data: try encoder.encode(value), status: status)
}
private func sceneManagementNameKey(_ name: String) -> [UInt8] {
    name.utf8.map { (65...90).contains($0) ? $0 + 32 : $0 }
}
private func sceneManagementDefinition(_ fields: [String: JSONValue]) throws -> SceneDefinition {
    try DaemonClient.decode(SceneDefinition.self, from: JSONEncoder().encode(fields))
}
private func sceneManagementLevels(_ level: Int = 24) throws -> SceneDefinition {
    try sceneManagementDefinition([
        "groups": .array([]), "volumes": .object(["Studio@TEST-B": .integer(Int64(level))]),
        "mutes": .object(["Hall@TEST-A": .bool(false)])
    ])
}
private func sceneManagementSources() -> SceneDraft {
    var draft = SceneDraft()
    draft.name = "Sources"
    var favorite = SceneGroupDraft()
    favorite.coordinator = sceneManagementRoom
    favorite.source = .favorite; favorite.value = "favorite-a"; favorite.playback = .paused
    var dj = SceneGroupDraft()
    dj.coordinator = sceneManagementOther
    dj.source = .dj; dj.value = "CALM"; dj.playback = .playing
    draft.groups = [favorite, dj]
    return draft
}

/// The fixture only changes stored scene definitions. All reviews, dependency
/// observations, dispatch, receipts, and cancellation use production code.
private actor SceneManagementTransport: DaemonTransport {
    var requests: [URLRequest] = []
    var rows = [sceneManagementSaved()]
    var topology = [sceneManagementZone(), sceneManagementZone("TEST-B")]
    var favorites = [Favorite(id: "favorite-a", title: "Evening Radio", kind: "stream", description: "Station")]
    var moods = [MoodCatalog.Mood(name: "calm", constraints: ["energy": .string("low")])]
    var failPath: String?
    var writeOverride: HTTPReply?
    var loseReply = false
    var cancelReply = false
    var saves = 0
    var deletes = 0
    var blocked = false
    private var holdKey: String?
    private var held: CheckedContinuation<Void, Never>?

    func setRows(_ value: [Scene]) { rows = value }
    func setTopology(_ value: [Zone]) { topology = value }
    func setFavorites(_ value: [Favorite]) { favorites = value }
    func setMoods(_ value: [MoodCatalog.Mood]) { moods = value }
    func fail(_ value: String?) { failPath = value }
    func overrideWrite(_ value: HTTPReply?) { writeOverride = value }
    func loseNextReply() { loseReply = true }
    func cancelNextReply() { cancelReply = true }
    func hold(_ method: String, _ path: String) { holdKey = method + " " + path }
    func release() { let saved = held; held = nil; blocked = false; saved?.resume() }
    func writes() -> [URLRequest] { requests.filter { ($0.httpMethod ?? "GET") != "GET" } }
    func send(_ request: URLRequest) async throws -> HTTPReply {
        requests.append(request)
        let method = request.httpMethod ?? "GET"
        let path = request.url!.path.removingPercentEncoding ?? request.url!.path
        if method == "GET", failPath == path { throw URLError(.cannotConnectToHost) }
        let response: HTTPReply
        if method == "GET" {
            switch path {
            case "/scenes": response = try sceneManagementJSON(rows)
            case "/zones": response = try sceneManagementJSON(topology)
            case "/favorites": response = try sceneManagementJSON(favorites)
            case "/dj/moods": response = try sceneManagementJSON(["moods": moods])
            case "/schedules": response = try sceneManagementJSON([scheduleFixture()])
            default:
                if path.hasPrefix("/scenes/") {
                    let name = String(path.dropFirst("/scenes/".count))
                    guard let row = rows.first(where: { sceneManagementNameKey($0.name) == sceneManagementNameKey(name) }) else {
                        return HTTPReply(data: Data(#"{"code":"UNKNOWN_SCENE","detail":"Scene no longer exists"}"#.utf8), status: 404)
                    }
                    response = try sceneManagementJSON(row)
                } else if path.hasSuffix("/state"),
                          let zone = topology.first(where: { path == "/zones/" + $0.id.address + "/state" }) {
                    response = try sceneManagementJSON(ZoneState(zone: zone, transportState: "playing", volume: 24,
                        mute: false, groupVolume: 24, track: nil, shuffle: false, repeat: "off"))
                } else if path.hasSuffix("/queue") {
                    response = try sceneManagementJSON(QueuePage(total: 0, items: [], playing: nil, revision: "test-queue"))
                } else { throw ContractError.invalid("Unexpected scene test read: " + path) }
            }
        } else if method == "PUT", path.hasPrefix("/scenes/") {
            let name = String(path.dropFirst("/scenes/".count))
            let body = request.httpBody.flatMap { try? JSONDecoder().decode([String: JSONValue].self, from: $0) } ?? [:]
            let saved: Scene
            if body.isEmpty { saved = sceneManagementCaptured(name) }
            else {
                let fields = body.merging(["name": .string(name), "saved": .integer(1_800_000_000)]) { _, new in new }
                let definition = try DaemonClient.decode(Scene.self, from: JSONEncoder().encode(fields))
                let groups = definition.groups.map { group in
                    var source = group.source
                    if source.kind == "favorite", let favorite = favorites.first(where: { $0.id == source.name || $0.title == source.name }) {
                        source = .init(kind: "favorite", name: favorite.title, uri: "x-sonosapi-stream:synthetic-station", mood: nil)
                    }
                    return Scene.Group(coordinator: group.coordinator, members: group.members, source: source, playing: group.playing)
                }
                saved = Scene(name: name, saved: definition.saved, groups: groups, volumes: definition.volumes, mutes: definition.mutes)
            }
            saves += 1
            rows.removeAll { sceneManagementNameKey($0.name) == sceneManagementNameKey(name) }
            rows.append(saved)
            response = try writeOverride ?? sceneManagementJSON(saved)
        } else if method == "DELETE", path.hasPrefix("/scenes/") {
            let name = String(path.dropFirst("/scenes/".count))
            guard let index = rows.firstIndex(where: { sceneManagementNameKey($0.name) == sceneManagementNameKey(name) }) else {
                return HTTPReply(data: Data(#"{"code":"UNKNOWN_SCENE","detail":"Scene no longer exists"}"#.utf8), status: 404)
            }
            let removed = rows.remove(at: index)
            deletes += 1
            response = try writeOverride ?? sceneManagementJSON(["deleted": removed.name, "done": "Deleted saved scene"])
        } else {
            throw ContractError.invalid("Unexpected scene test write: " + method + " " + path)
        }
        if holdKey == method + " " + path {
            holdKey = nil; blocked = true
            await withCheckedContinuation { held = $0 }
        }
        if method != "GET" && loseReply { loseReply = false; throw URLError(.networkConnectionLost) }
        if method != "GET" && cancelReply { cancelReply = false; throw CancellationError() }
        return response
    }
    nonisolated func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> {
        AsyncThrowingStream { $0.yield(.connected) }
    }
}
private func sceneManagementClient(_ transport: SceneManagementTransport) throws -> DaemonClient {
    DaemonClient(endpoint: try DaemonEndpoint("https://music.example.test"), transport: transport)
}
private func sceneManagementBody(_ request: URLRequest) throws -> [String: JSONValue] {
    guard let body = request.httpBody, !body.isEmpty else { return [:] }
    return try JSONDecoder().decode([String: JSONValue].self, from: body)
}
private func sceneManagementGate(_ transport: SceneManagementTransport) async throws {
    let deadline = ContinuousClock.now.advanced(by: .seconds(3))
    while !(await transport.blocked) {
        guard ContinuousClock.now < deadline else { throw ContractError.invalid("Scene test gate was not reached") }
        try await Task.sleep(for: .milliseconds(3))
    }
}
private func sceneManagementUncertain(_ session: CommandReviewSession, _ review: SceneMutationReview) async {
    do {
        _ = try await session.execute(review)
        Issue.record("A lost or contradictory scene acknowledgement was reported as success")
    } catch {
        if case ClientError.uncertain = error { }
        else { Issue.record("A dispatched scene mutation must retain uncertainty: \(error)") }
    }
}

@Test func nativeSceneManagementEmptyDefinitionCannotAccidentallyCaptureTheHouse() async throws {
    var draft = SceneDraft(); draft.name = "Empty"
    #expect(throws: ContractError.self) { try draft.definition(zones: [sceneManagementZone()]) }
    let transport = SceneManagementTransport(), session = try CommandReviewSession(client: sceneManagementClient(transport))
    let empty = try sceneManagementDefinition(["groups": .array([]), "volumes": .object([:]), "mutes": .object([:])])
    await #expect(throws: (any Error).self) { try await session.prepare(SceneMutation.save(name: "Empty", definition: empty, replacing: nil)) }
    #expect(await transport.writes().isEmpty)
}

@Test func nativeSceneManagementNameLengthCountsUnicodeScalarsAndTrimsBeforeSaving() async throws {
    let valid = String(repeating: "e\u{301}", count: 32)
    var draft = SceneDraft(); draft.name = valid; draft.volumes[sceneManagementRoom] = 20
    _ = try draft.definition(zones: [sceneManagementZone()])
    draft.name = valid + "e\u{301}"
    #expect(throws: (any Error).self) { try draft.definition(zones: [sceneManagementZone()]) }
    let transport = SceneManagementTransport(), session = try CommandReviewSession(client: sceneManagementClient(transport))
    let review = try await session.prepare(SceneMutation.capture(name: " \n" + valid + "\t ", replacing: nil))
    let receipt = try await session.execute(review)
    #expect(receipt.saved?.name == valid)
    #expect(await transport.writes().first?.url?.lastPathComponent == valid)
}

@Test func nativeSceneManagementASCIINameCollisionsAreAmbiguousButUnicodeCaseIsDistinct() async throws {
    let transport = SceneManagementTransport(), client = try sceneManagementClient(transport)
    await transport.setRows([sceneManagementSaved("Evening"), sceneManagementSaved("EVENING")])
    await #expect(throws: ContractError.self) { try await client.scenes() }
    await transport.setRows([sceneManagementSaved("Été"), sceneManagementSaved("été")])
    #expect(try await client.scenes().count == 2)
    #expect(await transport.writes().isEmpty)
}

@Test func nativeSceneManagementUnicodeNormalizationVariantsRemainDistinctStoredScenes() async throws {
    let composed = "Caf\u{e9}", decomposed = "Cafe\u{301}"
    let transport = SceneManagementTransport(), client = try sceneManagementClient(transport)
    let original = sceneManagementSaved(composed)
    await transport.setRows([original])
    let session = CommandReviewSession(client: client)
    let review = try await session.prepare(SceneMutation.capture(name: decomposed, replacing: nil))
    #expect(review.previous == nil)
    let receipt = try await session.execute(review)
    #expect(receipt.saved?.name.utf8.elementsEqual(decomposed.utf8) == true)
    let scenes = try await client.scenes()
    #expect(scenes.count == 2)
    #expect(Set(scenes.map(\.id)).count == 2)
    #expect(scenes.filter { $0.name.utf8.elementsEqual(composed.utf8) }.count == 1)
    #expect(scenes.filter { $0.name.utf8.elementsEqual(decomposed.utf8) }.count == 1)
    #expect(try await client.scene(named: decomposed).name.utf8.elementsEqual(decomposed.utf8))
    await #expect(throws: ContractError.self) {
        try await session.prepare(SceneMutation.capture(name: decomposed, replacing: original))
    }
    #expect(await transport.saves == 1)
}

@Test func nativeSceneManagementReceiptMustPreserveTheExactUnicodeNameBytes() async throws {
    let composed = "Caf\u{e9}", decomposed = "Cafe\u{301}"
    let transport = SceneManagementTransport(), session = try CommandReviewSession(client: sceneManagementClient(transport))
    await transport.setRows([])
    let review = try await session.prepare(SceneMutation.capture(name: decomposed, replacing: nil))
    await transport.overrideWrite(try sceneManagementJSON(sceneManagementCaptured(composed)))
    await sceneManagementUncertain(session, review)
    #expect(await transport.saves == 1)
}

@Test func nativeSceneManagementCaptureUsesServerOwnedStateAndAnEncodedSinglePathElement() async throws {
    let transport = SceneManagementTransport(), session = try CommandReviewSession(client: sceneManagementClient(transport))
    let name = "Captured / quiet + café ? #"
    let review = try await session.prepare(SceneMutation.capture(name: name, replacing: nil))
    #expect(await transport.writes().isEmpty)
    let receipt = try await session.execute(review)
    #expect(receipt.saved == sceneManagementCaptured(name) && receipt.deleted == nil)
    let request = try #require(await transport.writes().first)
    #expect(request.httpMethod == "PUT" && request.url?.query == nil && request.url?.fragment == nil)
    #expect(request.url?.absoluteString.contains("%2F") == true)
    #expect(try sceneManagementBody(request).isEmpty)
    await #expect(throws: ContractError.self) { try await session.execute(review) }
    #expect(await transport.saves == 1)
}

@Test func nativeSceneManagementExplicitSaveUsesExactDefinitionWithoutPlaybackOrOwnershipFields() async throws {
    let transport = SceneManagementTransport(), session = try CommandReviewSession(client: sceneManagementClient(transport))
    let definition = try sceneManagementLevels()
    let review = try await session.prepare(SceneMutation.save(name: "Levels", definition: definition, replacing: nil))
    #expect(review.preview?.volumes == ["Studio@TEST-B": 24])
    let receipt = try await session.execute(review)
    #expect(receipt.saved?.name == "Levels" && receipt.saved?.groups.isEmpty == true)
    #expect(receipt.saved?.mutes == ["Hall@TEST-A": false])
    let request = try #require(await transport.writes().first)
    #expect(request.httpMethod == "PUT")
    #expect(try sceneManagementBody(request) == [
        "groups": .array([]), "volumes": .object(["Studio@TEST-B": .integer(24)]),
        "mutes": .object(["Hall@TEST-A": .bool(false)])
    ])
    #expect(await transport.requests.filter { $0.url!.path.hasSuffix("/state") }.isEmpty)
    #expect(await transport.writes().count == 1)
}

@Test func nativeSceneManagementKeepSourceRetainsUnchangedPlaybackAndExplicitSoloMembership() async throws {
    var draft = SceneDraft(); draft.name = "Solo"
    var group = SceneGroupDraft(); group.coordinator = sceneManagementRoom
    group.source = .keep; group.playback = .unchanged
    draft.groups = [group]
    let definition = try draft.definition(zones: [sceneManagementZone()])
    let transport = SceneManagementTransport(), session = try CommandReviewSession(client: sceneManagementClient(transport))
    let reviewed = try await session.prepare(SceneMutation.save(name: draft.name, definition: definition, replacing: nil))
    let receipt = try await session.execute(reviewed)
    let saved = try #require(receipt.saved?.groups.first)
    #expect(saved.coordinator == "Studio@TEST-A" && saved.members.isEmpty && saved.playing == nil)
    #expect(saved.source.kind == "keep")
    let body = try sceneManagementBody(#require(await transport.writes().first))
    guard case .array(let groups) = body["groups"], case .object(let wire) = groups.first else {
        Issue.record("Missing group definition"); return
    }
    #expect(wire["playing"] == nil)
    #expect(wire["members"] == .array([]))
}

@Test func nativeSceneManagementFavoritesAndMoodsAreResolvedInTheirOwnHouseholds() async throws {
    let draft = sceneManagementSources()
    let definition = try draft.definition(zones: [sceneManagementZone(), sceneManagementZone("TEST-B")])
    let transport = SceneManagementTransport(), session = try CommandReviewSession(client: sceneManagementClient(transport))
    let review = try await session.prepare(SceneMutation.save(name: draft.name, definition: definition, replacing: nil))
    #expect(review.preview?.groups.first?.source.name == "Evening Radio")
    #expect(review.preview?.groups.last?.source.mood == "calm")
    let receipt = try await session.execute(review)
    #expect(receipt.saved?.groups.first?.source.uri == "x-sonosapi-stream:synthetic-station")
    #expect(receipt.saved?.groups.first?.playing == false && receipt.saved?.groups.last?.playing == true)
    let requests = await transport.requests
    let favorite = try #require(requests.first { $0.url?.path == "/favorites" })
    let moods = try #require(requests.first { $0.url?.path == "/dj/moods" })
    #expect(URLComponents(url: favorite.url!, resolvingAgainstBaseURL: false)?.queryItems?.first { $0.name == "zone" }?.value == "Studio@TEST-A")
    #expect(URLComponents(url: moods.url!, resolvingAgainstBaseURL: false)?.queryItems?.first { $0.name == "zone" }?.value == "Studio@TEST-B")
    #expect(await transport.writes().count == 1)
}

@Test func nativeSceneManagementSourceDependencyChangesInvalidateTheReviewedDefinition() async throws {
    for favoriteChanged in [true, false] {
        let draft = sceneManagementSources()
        let definition = try draft.definition(zones: [sceneManagementZone(), sceneManagementZone("TEST-B")])
        let transport = SceneManagementTransport(), session = try CommandReviewSession(client: sceneManagementClient(transport))
        let review = try await session.prepare(SceneMutation.save(name: draft.name, definition: definition, replacing: nil))
        if favoriteChanged {
            await transport.setFavorites([Favorite(id: "favorite-a", title: "Different station", kind: "stream", description: "Changed")])
        } else {
            await transport.setMoods([MoodCatalog.Mood(name: "calm", constraints: ["energy": .string("high")])])
        }
        await #expect(throws: (any Error).self) { try await session.execute(review) }
        #expect(await transport.writes().isEmpty)
    }
}

@Test func nativeSceneManagementCaseOnlyReplacementIsReviewedAndReturnsTheRequestedSpelling() async throws {
    let transport = SceneManagementTransport(), session = try CommandReviewSession(client: sceneManagementClient(transport))
    let original = sceneManagementSaved("Evening")
    await transport.setRows([original])
    let review = try await session.prepare(SceneMutation.capture(name: "EVENING", replacing: original))
    #expect(review.previous == original && review.name == "EVENING")
    let receipt = try await session.execute(review)
    #expect(receipt.saved?.name == "EVENING")
    #expect(await transport.rows.count == 1)
    #expect(await transport.writes().count == 1)
}

@Test func nativeSceneManagementCreateDoesNotOverwriteAnUnreviewedCaseInsensitiveCollision() async throws {
    for appearsDuringReview in [true, false] {
        let transport = SceneManagementTransport(), session = try CommandReviewSession(client: sceneManagementClient(transport))
        await transport.setRows([])
        if appearsDuringReview {
            let review = try await session.prepare(SceneMutation.capture(name: "Evening", replacing: nil))
            await transport.setRows([sceneManagementSaved("EVENING")])
            await #expect(throws: (any Error).self) { try await session.execute(review) }
        } else {
            await transport.setRows([sceneManagementSaved("EVENING")])
            await #expect(throws: (any Error).self) { try await session.prepare(SceneMutation.capture(name: "Evening", replacing: nil)) }
        }
        #expect(await transport.writes().isEmpty)
    }
}

@Test func nativeSceneManagementReplacementOrDeletionNeverTargetsAChangedDefinition() async throws {
    for removing in [true, false] {
        for missing in [true, false] {
            let transport = SceneManagementTransport(), session = try CommandReviewSession(client: sceneManagementClient(transport))
            let original = sceneManagementSaved()
            let operation: SceneMutation = removing ? .delete(original) : .save(name: original.name, definition: try sceneManagementLevels(), replacing: original)
            let review = try await session.prepare(operation)
            await transport.setRows(missing ? [] : [sceneManagementSaved(level: 55, at: 2)])
            await #expect(throws: (any Error).self) { try await session.execute(review) }
            await #expect(throws: ContractError.self) { try await session.execute(review) }
            #expect(await transport.writes().isEmpty)
        }
    }
}

@Test func nativeSceneManagementListingAndDeletionNeedNoSpeakerOrSelectedRoom() async throws {
    let transport = SceneManagementTransport(), client = try sceneManagementClient(transport)
    await transport.fail("/zones")
    let session = CommandReviewSession(client: client)
    let saved = try #require(try await client.scenes().first)
    let review = try await session.prepare(SceneMutation.delete(saved))
    let receipt = try await session.execute(review)
    #expect(receipt.deleted == saved.name && receipt.saved == nil)
    let writes = await transport.writes()
    #expect(writes.count == 1 && writes[0].httpMethod == "DELETE")
    #expect(try sceneManagementBody(writes[0]).isEmpty)
    #expect(await transport.requests.allSatisfy { $0.url!.path.hasPrefix("/scenes") })
    #expect(await transport.rows.isEmpty)
}

@Test func nativeSceneManagementDraftRejectsCrossHouseholdGroupsAndInvalidRoomSettings() throws {
    let zones = [sceneManagementZone(), sceneManagementZone("TEST-B")]
    var draft = SceneDraft(); draft.name = "Invalid"
    var group = SceneGroupDraft(); group.coordinator = sceneManagementRoom
    group.members = [sceneManagementOther]; draft.groups = [group]
    #expect(throws: (any Error).self) { try draft.definition(zones: zones) }
    group.members = []; draft.groups = [group, group]
    #expect(throws: (any Error).self) { try draft.definition(zones: zones) }
    draft.groups = []; draft.volumes[sceneManagementRoom] = 101
    #expect(throws: (any Error).self) { try draft.definition(zones: zones) }
    draft.volumes = [RoomID(household: "TEST-UNKNOWN", room: "Studio"): 20]
    #expect(throws: (any Error).self) { try draft.definition(zones: zones) }
}

@Test func nativeSceneManagementSpotifySourcesAreCanonicalBeforeTheDefinitionIsSaved() async throws {
    var draft = SceneDraft(); draft.name = "Album"
    var group = SceneGroupDraft()
    group.coordinator = sceneManagementRoom; group.source = .uri; group.playback = .playing
    group.value = "https://open.spotify.com/intl-en/album/0123456789012345678901?si=test"
    draft.groups = [group]
    let definition = try draft.definition(zones: [sceneManagementZone()])
    let transport = SceneManagementTransport(), session = try CommandReviewSession(client: sceneManagementClient(transport))
    let review = try await session.prepare(SceneMutation.save(name: draft.name, definition: definition, replacing: nil))
    #expect(review.preview?.groups.first?.source.uri == "spotify:album:0123456789012345678901")
    let receipt = try await session.execute(review)
    #expect(receipt.saved?.groups.first?.source.uri == "spotify:album:0123456789012345678901")
    #expect(await transport.writes().count == 1)
    draft.groups[0].value = "https://open.spotify.com/?playlist/0123456789012345678901"
    #expect(throws: ContractError.self) { try draft.definition(zones: [sceneManagementZone()]) }
}

@Test func nativeSceneManagementMissingReadsDoNotMeanAnEmptyCatalogOrConsentToOverwrite() async throws {
    let transport = SceneManagementTransport(), session = try CommandReviewSession(client: sceneManagementClient(transport))
    await transport.fail("/scenes")
    await #expect(throws: (any Error).self) { try await session.prepare(SceneMutation.capture(name: "New", replacing: nil)) }
    await transport.fail(nil)
    let reviewed = try await session.prepare(SceneMutation.delete(sceneManagementSaved()))
    await transport.fail("/scenes")
    await #expect(throws: (any Error).self) { try await session.execute(reviewed) }
    #expect(await transport.writes().isEmpty)
}

@Test func nativeSceneManagementAChangedDestinationGroupRequiresAnotherReview() async throws {
    let transport = SceneManagementTransport(), session = try CommandReviewSession(client: sceneManagementClient(transport))
    let review = try await session.prepare(SceneMutation.save(name: "Levels", definition: sceneManagementLevels(), replacing: nil))
    await transport.setTopology([sceneManagementZone(),
        Zone(household: "TEST-B", coordinatorRoom: "Hall", members: ["Studio", "Hall"], transportState: "playing")])
    await #expect(throws: (any Error).self) { try await session.execute(review) }
    #expect(await transport.writes().isEmpty)
}

@Test func nativeSceneManagementSaveReceiptMustConfirmEveryAuthoredField() async throws {
    let badReplies = [
        Scene(name: "Wrong name", saved: 2, groups: [], volumes: ["Studio@TEST-B": 24], mutes: ["Hall@TEST-A": false]),
        Scene(name: "Levels", saved: 2, groups: [], volumes: ["Studio@TEST-B": 55], mutes: ["Hall@TEST-A": false]),
        Scene(name: "Levels", saved: 2, groups: [], volumes: ["Studio@TEST-A": 24], mutes: ["Hall@TEST-A": false]),
        Scene(name: "Levels", saved: 2, groups: [], volumes: ["Studio@TEST-B": 24], mutes: ["Hall@TEST-A": true])
    ]
    for saved in badReplies {
        let transport = SceneManagementTransport(), session = try CommandReviewSession(client: sceneManagementClient(transport))
        let review = try await session.prepare(SceneMutation.save(name: "Levels", definition: sceneManagementLevels(), replacing: nil))
        await transport.overrideWrite(try sceneManagementJSON(saved))
        await sceneManagementUncertain(session, review)
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes().count == 1)
    }
}

@Test func nativeSceneManagementDeleteReceiptCannotClaimADifferentSceneWasRemoved() async throws {
    let wrongReplies = [
        HTTPReply(data: Data(#"{"deleted":"Another scene","done":"Deleted"}"#.utf8), status: 200),
        HTTPReply(data: Data(#"{"deleted":"Evening / café & friends","done":""}"#.utf8), status: 200),
        HTTPReply(data: Data("invalid".utf8), status: 200)
    ]
    for reply in wrongReplies {
        let transport = SceneManagementTransport(), session = try CommandReviewSession(client: sceneManagementClient(transport))
        let review = try await session.prepare(SceneMutation.delete(sceneManagementSaved()))
        await transport.overrideWrite(reply)
        await sceneManagementUncertain(session, review)
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.deletes == 1)
    }
}

@Test func nativeSceneManagementLostCaptureAndDeleteAcknowledgementsNeverRepeatTheMutation() async throws {
    for deleting in [true, false] {
        let transport = SceneManagementTransport(), session = try CommandReviewSession(client: sceneManagementClient(transport))
        let command: SceneMutation = deleting ? .delete(sceneManagementSaved()) : .capture(name: "Capture", replacing: nil)
        let review = try await session.prepare(command)
        await transport.loseNextReply()
        await sceneManagementUncertain(session, review)
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes().count == 1)
        if deleting { #expect(await transport.deletes == 1) }
        else { #expect(await transport.rows.contains { $0.name == "Capture" }) }
    }
}

@Test func nativeSceneManagementCancellationDuringFinalReadKeepsTheSlotUntilItDrains() async throws {
    let transport = SceneManagementTransport(), session = try CommandReviewSession(client: sceneManagementClient(transport))
    let review = try await session.prepare(SceneMutation.delete(sceneManagementSaved()))
    await transport.hold("GET", "/scenes")
    let confirmation = Task { try await session.execute(review) }
    try await sceneManagementGate(transport)
    await session.cancel()
    await #expect(throws: ContractError.self) { try await session.prepare(.pause, in: sceneManagementZone()) }
    await #expect(throws: ContractError.self) { try await session.prepareUndo() }
    await transport.release()
    await #expect(throws: CancellationError.self) { try await confirmation.value }
    await #expect(throws: ContractError.self) { try await session.execute(review) }
    #expect(await transport.writes().isEmpty)
}

@Test func nativeSceneManagementDuplicateConfirmationAndCancellationDoNotRepeatASentWrite() async throws {
    let transport = SceneManagementTransport(), session = try CommandReviewSession(client: sceneManagementClient(transport))
    let review = try await session.prepare(SceneMutation.capture(name: "Capture", replacing: nil))
    await transport.hold("PUT", "/scenes/Capture")
    let confirmation = Task { try await session.execute(review) }
    try await sceneManagementGate(transport)
    await #expect(throws: ContractError.self) { try await session.execute(review) }
    await #expect(throws: ContractError.self) { try await session.prepare(SceneMutation.delete(sceneManagementSaved())) }
    await #expect(throws: ContractError.self) { try await session.prepare(.pause, in: sceneManagementZone()) }
    confirmation.cancel(); await session.cancel()
    await transport.release()
    #expect(try await confirmation.value.saved?.name == "Capture")
    #expect(await transport.saves == 1)
}

@Test func nativeSceneManagementTransportCancellationAfterDispatchIsUncertain() async throws {
    let transport = SceneManagementTransport(), session = try CommandReviewSession(client: sceneManagementClient(transport))
    let review = try await session.prepare(SceneMutation.capture(name: "Capture", replacing: nil))
    await transport.cancelNextReply()
    await sceneManagementUncertain(session, review)
    await #expect(throws: ContractError.self) { try await session.execute(review) }
    #expect(await transport.saves == 1)
}

@Test func nativeSceneManagementFadesApplyOnlyTheReviewedDurationAndNeverRetarget() async throws {
    let fades: [UInt32?] = [nil, 1, 60]
    for fade in fades {
        let transport = NativeAutomationTransport(), session = try CommandReviewSession(client: automationClient(transport))
        let scene = automationScene()
        let review = try await session.prepare(.scene(scene, fadeSeconds: fade), in: automationZone())
        _ = try await session.execute(review)
        let request = try #require(await transport.writes().first)
        let expected: [String: JSONValue] = fade.map { ["fade_secs": .integer(Int64($0))] } ?? [:]
        #expect(try sceneManagementBody(request) == expected)
        #expect(request.httpMethod == "POST" && request.url?.query == nil)
        #expect(request.url?.path.hasSuffix("/apply") == true)
        #expect(review.command.savedScene == scene)
        #expect(await transport.writes().count == 1)
    }
}

@Test func nativeSceneManagementUnsupportedFadesAreRejectedBeforeReadingOrSending() async throws {
    let fades: [UInt32] = [0, 61, .max]
    for fade in fades {
        let transport = NativeAutomationTransport(), session = try CommandReviewSession(client: automationClient(transport))
        await #expect(throws: ContractError.self) {
            try await session.prepare(.scene(automationScene(), fadeSeconds: fade), in: automationZone())
        }
        #expect(await transport.requests.isEmpty)
    }
}

@MainActor private func sceneManagementScreenWait(_ condition: () -> Bool) async throws {
    let deadline = ContinuousClock.now.advanced(by: .seconds(3))
    while !condition() {
        guard ContinuousClock.now < deadline else { throw ContractError.invalid("Scene management screen did not settle") }
        try await Task.sleep(for: .milliseconds(5))
    }
}

@MainActor private func sceneManagementScreen(_ transport: SceneManagementTransport, waitForRooms: Bool = true) async throws -> RemoteModel {
    let model = try RemoteModel(client: sceneManagementClient(transport))
    model.show(.scenes); model.resume()
    try await sceneManagementScreenWait { model.canManageScenes && (!waitForRooms || model.selectedZone != nil) }
    return model
}

@Test @MainActor func nativeSceneManagementScreenCanDeleteWithNoSelectedOrReachableSpeaker() async throws {
    let transport = SceneManagementTransport()
    await transport.fail("/zones")
    let model = try await sceneManagementScreen(transport, waitForRooms: false)
    defer { model.stop() }
    #expect(model.selectedRoom == nil && !model.canPrepare)
    let original = try #require(model.scenes.first)
    model.prepareSceneDelete(original)
    try await sceneManagementScreenWait { model.sceneMutationReview != nil && model.actionPhase == .idle }
    #expect(model.sceneMutationReview?.previous == original && model.sceneMutationReview?.groups.isEmpty == true)
    let id = try #require(model.sceneMutationReview?.id)
    model.confirmSceneMutation(id); model.confirmSceneMutation(id)
    try await sceneManagementScreenWait { model.sceneMutationResult != nil && model.actionPhase == .idle }
    #expect(model.sceneMutationResult?.kind == .confirmed)
    #expect(model.sceneMutationResult?.command.name == original.name && model.sceneMutationResult?.receipt?.deleted == original.name)
    #expect(model.result == nil && model.scheduleResult == nil && model.undoResult == nil)
    #expect(await transport.deletes == 1)
}

@Test @MainActor func nativeSceneManagementScreenStartsReplacementBlankAndRequiresExplicitSettings() async throws {
    let transport = SceneManagementTransport(), model = try await sceneManagementScreen(transport)
    defer { model.stop() }
    let original = try #require(model.scenes.first)
    model.beginSceneDraft(replacing: original)
    #expect(model.sceneComposerOpen && model.sceneDraft.name == original.name)
    #expect(model.sceneDraft.groups.isEmpty && model.sceneDraft.volumes.isEmpty && model.sceneDraft.mutes.isEmpty)
    #expect(model.sceneDraftReplacing == original)
    model.prepareSceneDraft()
    #expect(model.sceneDraftProblem != nil && model.sceneMutationReview == nil)
    #expect(await transport.writes().isEmpty)
    var draft = model.sceneDraft
    draft.volumes[sceneManagementOther] = 28; draft.mutes[sceneManagementRoom] = false
    model.updateSceneDraft(draft); model.prepareSceneDraft()
    try await sceneManagementScreenWait { model.sceneMutationReview != nil && model.actionPhase == .idle }
    #expect(model.sceneMutationReview?.previous == original)
    #expect(model.sceneMutationReview?.preview?.groups.isEmpty == true)
    let id = try #require(model.sceneMutationReview?.id)
    draft.volumes[sceneManagementOther] = 55; model.updateSceneDraft(draft)
    #expect(model.sceneDraft.volumes[sceneManagementOther] == 28)
    model.confirmSceneMutation(id)
    try await sceneManagementScreenWait { model.sceneMutationResult != nil && model.actionPhase == .idle }
    #expect(model.sceneMutationResult?.kind == .confirmed && !model.sceneComposerOpen)
    #expect(model.sceneMutationResult?.receipt?.saved?.volumes == ["Studio@TEST-B": 28])
    #expect(model.sceneMutationResult?.receipt?.saved?.groups.isEmpty == true)
    #expect(await transport.saves == 1)
}

@Test @MainActor func nativeSceneManagementScreenCaptureReviewsTheExistingCaseInsensitiveNameAndSharesTheWriteGate() async throws {
    let transport = SceneManagementTransport()
    let previous = sceneManagementSaved("Evening")
    await transport.setRows([previous])
    let model = try await sceneManagementScreen(transport)
    defer { model.stop() }
    model.updateSceneCaptureName("  EVENING  "); model.prepareSceneCapture()
    try await sceneManagementScreenWait { model.sceneMutationReview != nil && model.actionPhase == .idle }
    #expect(model.sceneMutationReview?.previous == previous && model.sceneMutationReview?.name == "EVENING")
    model.prepare(.pause); model.prepareUndo()
    model.prepareSchedule(.change(try scheduleDecode(scheduleFixture()), .pause))
    model.updateSceneCaptureName("Different scene")
    #expect(model.review == nil && model.undoReview == nil && model.scheduleReview == nil)
    #expect(!model.canPrepare && !model.canSchedule && !model.canManageScenes)
    #expect(model.sceneCaptureName == "  EVENING  ")
    let id = try #require(model.sceneMutationReview?.id)
    model.confirmSceneMutation(id); model.confirmSceneMutation(id)
    try await sceneManagementScreenWait { model.sceneMutationResult != nil && model.actionPhase == .idle }
    #expect(model.sceneMutationResult?.kind == .confirmed)
    #expect(model.sceneMutationResult?.receipt?.saved == sceneManagementCaptured("EVENING"))
    #expect(await transport.saves == 1)
}

@Test @MainActor func nativeSceneManagementScreenCannotAuthorOverAFailedCatalogRead() async throws {
    let transport = SceneManagementTransport()
    await transport.fail("/scenes")
    let model = try RemoteModel(client: sceneManagementClient(transport))
    defer { model.stop() }
    model.show(.scenes); model.resume()
    try await sceneManagementScreenWait { model.panelProblem != nil && !model.panelLoading }
    #expect(model.scenesObservedAt == nil && !model.canManageScenes)
    model.updateSceneCaptureName("Unavailable"); model.prepareSceneCapture()
    model.beginSceneDraft(); model.prepareSceneDelete(sceneManagementSaved())
    #expect(model.sceneMutationReview == nil && !model.sceneComposerOpen)
    #expect(await transport.writes().isEmpty)
}

@Test @MainActor func nativeSceneManagementScreenStaleDeletionRetainsItsOriginalReviewAndNeverReplays() async throws {
    let transport = SceneManagementTransport(), model = try await sceneManagementScreen(transport)
    defer { model.stop() }
    let original = try #require(model.scenes.first)
    model.prepareSceneDelete(original)
    try await sceneManagementScreenWait { model.sceneMutationReview != nil && model.actionPhase == .idle }
    let id = try #require(model.sceneMutationReview?.id)
    await transport.setRows([sceneManagementSaved(level: 44, at: 2)])
    model.confirmSceneMutation(id)
    try await sceneManagementScreenWait { model.sceneMutationResult != nil && model.actionPhase == .idle }
    #expect(model.sceneMutationResult?.kind == .notSent && model.sceneMutationResult?.command.previous == original)
    model.confirmSceneMutation(id); model.refresh()
    #expect(model.sceneMutationReview == nil && model.sceneMutationResult?.receipt == nil)
    #expect(await transport.writes().isEmpty)
}

@Test @MainActor func nativeSceneManagementScreenCancelledPreparationCannotReopenItsReview() async throws {
    let transport = SceneManagementTransport(), model = try await sceneManagementScreen(transport)
    defer { model.stop() }
    model.updateSceneCaptureName("Cancelled")
    await transport.hold("GET", "/scenes")
    model.prepareSceneCapture()
    try await sceneManagementGate(transport)
    model.cancelSceneMutationReview(); model.prepare(.pause); model.prepareSceneCapture()
    #expect(model.actionPhase == .preparing && model.review == nil && model.sceneMutationReview == nil)
    await transport.release()
    try await sceneManagementScreenWait { model.actionPhase == .idle }
    #expect(model.sceneMutationReview == nil && model.sceneMutationResult == nil)
    #expect(await transport.writes().isEmpty)
}

@Test @MainActor func nativeSceneManagementScreenSuspensionOrDismissalStopsTheFinalRead() async throws {
    for suspend in [true, false] {
        let transport = SceneManagementTransport(), model = try await sceneManagementScreen(transport)
        defer { model.stop() }
        model.prepareSceneDelete(try #require(model.scenes.first))
        try await sceneManagementScreenWait { model.sceneMutationReview != nil && model.actionPhase == .idle }
        let id = try #require(model.sceneMutationReview?.id)
        await transport.hold("GET", "/scenes")
        model.confirmSceneMutation(id)
        try await sceneManagementGate(transport)
        if suspend { model.suspend() } else { model.cancelSceneMutationReview() }
        await transport.release()
        try await sceneManagementScreenWait { model.sceneMutationResult != nil && model.actionPhase == .idle }
        #expect(model.sceneMutationResult?.kind == .notSent && model.sceneMutationResult?.command.name == sceneManagementName)
        #expect(model.sceneMutationReview == nil)
        model.confirmSceneMutation(id)
        #expect(await transport.writes().isEmpty)
    }
}

@Test @MainActor func nativeSceneManagementScreenLateReceiptKeepsItsOriginalSceneAfterSelectionOrSuspension() async throws {
    for suspend in [true, false] {
        let transport = SceneManagementTransport(), model = try await sceneManagementScreen(transport)
        defer { model.stop() }
        let name = "Captured before leaving"
        model.updateSceneCaptureName(name); model.prepareSceneCapture()
        try await sceneManagementScreenWait { model.sceneMutationReview != nil && model.actionPhase == .idle }
        let id = try #require(model.sceneMutationReview?.id)
        await transport.hold("PUT", "/scenes/" + name)
        model.confirmSceneMutation(id)
        try await sceneManagementGate(transport)
        if suspend { model.suspend() } else { model.select(sceneManagementOther) }
        model.confirmSceneMutation(id)
        await transport.release()
        try await sceneManagementScreenWait { model.sceneMutationResult != nil && model.actionPhase == .idle }
        #expect(model.sceneMutationResult?.kind == .confirmed && model.sceneMutationResult?.command.name == name)
        #expect(model.sceneMutationResult?.receipt?.saved == sceneManagementCaptured(name))
        if !suspend { #expect(model.selectedRoom == sceneManagementOther) }
        #expect(model.result == nil && model.scheduleResult == nil && model.undoResult == nil)
        #expect(await transport.saves == 1)
    }
}

@Test @MainActor func nativeSceneManagementScreenUnknownOutcomeSurvivesRefreshWithoutRepeatingTheCapture() async throws {
    let transport = SceneManagementTransport(), model = try await sceneManagementScreen(transport)
    defer { model.stop() }
    let name = "Captured without acknowledgement"
    model.updateSceneCaptureName(name); model.prepareSceneCapture()
    try await sceneManagementScreenWait { model.sceneMutationReview != nil && model.actionPhase == .idle }
    let id = try #require(model.sceneMutationReview?.id)
    await transport.loseNextReply()
    model.confirmSceneMutation(id)
    try await sceneManagementScreenWait { model.sceneMutationResult?.kind == .uncertain && model.actionPhase == .idle }
    model.confirmSceneMutation(id); model.refresh(); model.suspend(); model.resume()
    try await sceneManagementScreenWait { model.canManageScenes }
    #expect(model.sceneMutationResult?.kind == .uncertain && model.sceneMutationResult?.command.name == name)
    #expect(model.sceneMutationReview == nil && model.sceneMutationResult?.receipt == nil)
    #expect(await transport.rows.contains { $0.name == name })
    #expect(await transport.saves == 1)
    model.dismissSceneMutationResult()
    #expect(model.sceneMutationResult == nil)
    #expect(await transport.writes().count == 1)
}

@Test @MainActor func nativeSceneManagementScreenSourceChoicesNeverLeakAcrossHouseholdsOrSourceKinds() async throws {
    let transport = SceneManagementTransport(), model = try await sceneManagementScreen(transport)
    defer { model.stop() }
    model.beginSceneDraft()
    var draft = sceneManagementSources()
    draft.groups.removeLast()
    model.updateSceneDraft(draft)
    let id = draft.groups[0].id
    await transport.hold("GET", "/favorites")
    model.loadSceneChoices(for: id)
    try await sceneManagementGate(transport)
    draft.groups[0].coordinator = sceneManagementOther; draft.groups[0].source = .dj
    model.updateSceneDraft(draft); model.loadSceneChoices(for: id)
    #expect(model.sceneSourceChoices == nil && model.sceneChoicesLoading)
    await transport.release()
    try await sceneManagementScreenWait { model.sceneSourceChoices != nil && !model.sceneChoicesLoading }
    let choices = try #require(model.sceneSourceChoices)
    #expect(choices.coordinator == sceneManagementOther && choices.source == .dj)
    #expect(choices.favorites.isEmpty && choices.moods.first?.name == "calm")
    #expect(choices.matches(draft.groups[0]))
    let request = try #require(await transport.requests.last { $0.url?.path == "/dj/moods" })
    #expect(URLComponents(url: request.url!, resolvingAgainstBaseURL: false)?.queryItems?.first { $0.name == "zone" }?.value == "Studio@TEST-B")
    #expect(await transport.writes().isEmpty)
}
