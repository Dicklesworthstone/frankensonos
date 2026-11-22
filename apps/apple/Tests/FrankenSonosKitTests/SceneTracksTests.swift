import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

private let sceneTrackURI = "spotify:track:0000000000000000000001"
private let sceneEpisodeURI = "spotify:episode:0000000000000000000002"
private let sceneSelection: [Scene.Track] = [
    .init(sourceUri: sceneTrackURI, title: "Café opening"),
    .init(sourceUri: sceneEpisodeURI),
    .init(sourceUri: sceneTrackURI, title: "Reprise")
]

private let tracksSceneWire = #"""
{
  "name": "Ordered evening", "saved": 1,
  "groups": [{
    "coordinator": "Workshop@TEST-B", "members": ["Hall@TEST-B"], "playing": false,
    "source": {"kind": "tracks", "tracks": [
      {"source_uri": "spotify:track:0000000000000000000001", "title": "Café opening"},
      {"source_uri": "spotify:episode:0000000000000000000002"},
      {"source_uri": "spotify:track:0000000000000000000001", "title": "Reprise"}
    ]}
  }],
  "volumes": {}, "mutes": {}
}
"""#

private func tracksScene(tracks: [Scene.Track] = sceneSelection, playing: Bool? = false,
                         source: Scene.Source? = nil) -> Scene {
    Scene(name: "Ordered evening", saved: 1,
          groups: [.init(coordinator: "Workshop@TEST-B", members: ["Hall@TEST-B"],
                         source: source ?? .init(kind: "tracks", name: nil, uri: nil, mood: nil, tracks: tracks),
                         playing: playing)], volumes: [:], mutes: [:])
}

@Test func nativeTrackScenesDecodeAndRoundTripEveryOrderedOccurrenceAndOptionalTitle() throws {
    let saved = try DaemonClient.decode(Scene.self, from: Data(tracksSceneWire.utf8)).validated()
    #expect(saved == tracksScene())
    let source = try #require(saved.groups.first?.source)
    #expect(source.tracks == sceneSelection)
    #expect(source.tracksSummary == "3 selected tracks in order:\n1. Café opening [\(sceneTrackURI)]\n2. \(sceneEpisodeURI)\n3. Reprise [\(sceneTrackURI)]")
    #expect(source.tracksRecallNote.contains("replaces the group’s queue"))
    #expect(source.tracksRecallNote.contains("preserving order and repeats"))
    #expect(source.tracksRecallNote.contains("shuffle and repeat off"))
    #expect(source.tracksRecallNote.contains("complete verified scene on unchanged groups"))

    let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
    let encoded = try encoder.encode(saved)
    #expect(try DaemonClient.decode(Scene.self, from: encoded).validated() == saved)
    let json = try #require(JSONSerialization.jsonObject(with: encoded) as? [String: Any])
    let groups = try #require(json["groups"] as? [[String: Any]])
    let wire = try #require(groups[0]["source"] as? [String: Any])
    let entries = try #require(wire["tracks"] as? [[String: String]])
    #expect(entries.map { $0["source_uri"] } == [sceneTrackURI, sceneEpisodeURI, sceneTrackURI])
    #expect(entries[1]["title"] == nil && entries[2]["title"] == "Reprise")
    #expect(wire["uri"] == nil && wire["queue"] == nil && wire["playlist"] == nil)
    #expect(groups[0]["playing"] as? Bool == false)

    let legacy = try DaemonClient.decode(Scene.Source.self, from: Data(#"{"kind":"keep"}"#.utf8))
    #expect(legacy.tracks == nil && legacy.tracksSummary == nil)
    #expect(legacy == .init(kind: "keep", name: nil, uri: nil, mood: nil))
}

@Test func nativeTrackScenesValidateCanonicalKindsBoundsAndUnicodeTitlesLikeTheDaemon() throws {
    for count in [1, 10] {
        #expect(try tracksScene(tracks: Array(repeating: sceneSelection[0], count: count)).validated().groups[0].source.tracks?.count == count)
    }
    for count in [0, 11] {
        #expect(throws: ContractError.self) {
            try tracksScene(tracks: Array(repeating: sceneSelection[0], count: count)).validated()
        }
    }
    for uri in ["spotify:track:short", "spotify:album:0000000000000000000001",
                "spotify:playlist:0000000000000000000001", "spotify:artist:0000000000000000000001",
                "spotify:show:0000000000000000000001", "spotify:TRACK:0000000000000000000001",
                "spotify:track:000000000000000000000!", "spotify:track:000000000000000000000１",
                sceneTrackURI + "0", " " + sceneTrackURI, sceneTrackURI + "\n",
                "https://open.spotify.com/track/0000000000000000000001"] {
        #expect(throws: ContractError.self) {
            try tracksScene(tracks: [.init(sourceUri: uri)]).validated()
        }
    }
    for title in ["", " \t\r\n", "Hidden\u{0}title", "Hidden\u{7f}title",
                  String(repeating: "x", count: 257), String(repeating: "e\u{301}", count: 129)] {
        #expect(throws: ContractError.self) {
            try tracksScene(tracks: [.init(sourceUri: sceneTrackURI, title: title)]).validated()
        }
    }
    for title in ["Valid\tXML\r\nwhitespace", String(repeating: "x", count: 256),
                  String(repeating: "e\u{301}", count: 128)] {
        let saved = tracksScene(tracks: [.init(sourceUri: sceneTrackURI, title: title)])
        #expect(try saved.validated() == saved)
    }
    for field in ["tracks", "source_uri"] {
        let missing = tracksSceneWire.replacingOccurrences(of: "\"\(field)\":", with: "\"missing_\(field)\":")
        #expect(throws: (any Error).self) {
            try DaemonClient.decode(Scene.self, from: Data(missing.utf8)).validated()
        }
    }
}

@Test func nativeTrackScenesRejectMissingSelectionsAndMixedSourcesWithoutLosingSavedTracks() throws {
    let invalid: [Scene.Source] = [
        .init(kind: "tracks", name: nil, uri: nil, mood: nil),
        .init(kind: "tracks", name: "Favorite", uri: nil, mood: nil, tracks: sceneSelection),
        .init(kind: "tracks", name: nil, uri: sceneTrackURI, mood: nil, tracks: sceneSelection),
        .init(kind: "tracks", name: nil, uri: nil, mood: "calm", tracks: sceneSelection),
        .init(kind: "tracks", name: nil, uri: nil, mood: nil,
              queue: .init(coordinator: "RINCON_TEST", updateId: 1, track: 1, positionSecs: 0), tracks: sceneSelection),
        .init(kind: "tracks", name: nil, uri: nil, mood: nil,
              playlist: .init(household: "TEST-B", id: "SQ:7", title: "Evening"), tracks: sceneSelection),
        .init(kind: "keep", name: nil, uri: nil, mood: nil, tracks: sceneSelection),
        .init(kind: "favorite", name: "Evening", uri: nil, mood: nil, tracks: sceneSelection),
        .init(kind: "uri", name: nil, uri: sceneTrackURI, mood: nil, tracks: sceneSelection),
        .init(kind: "dj", name: nil, uri: nil, mood: "calm", tracks: sceneSelection)
    ]
    for source in invalid {
        #expect(throws: ContractError.self) { try tracksScene(source: source).validated() }
    }
    // The native composer authors fresh definitions. It cannot silently copy
    // or discard a selection that its form does not support editing.
    for source in invalid + [tracksScene().groups[0].source] {
        #expect(throws: ContractError.self) {
            try SceneDefinition(groups: tracksScene(source: source).groups)
                .validated(zones: [automationZone(house: "TEST-B")])
        }
    }
}

@Test func nativeTrackSceneReviewDetectsChangedOrderOccurrencesTitlesAndPlaybackBeforeWriting() async throws {
    var reordered = sceneSelection; reordered.swapAt(0, 1)
    var renamed = sceneSelection
    renamed[0] = .init(sourceUri: sceneTrackURI, title: "Cafe\u{301} opening")
    #expect(renamed[0].title == sceneSelection[0].title)
    #expect(renamed[0] != sceneSelection[0])
    let changed = [tracksScene(tracks: reordered), tracksScene(tracks: Array(sceneSelection.prefix(2))),
                   tracksScene(tracks: renamed), tracksScene(playing: true),
                   tracksScene(tracks: [.init(sourceUri: "spotify:track:0000000000000000000003")])]
    for replacement in changed {
        let transport = NativeAutomationTransport(), client = try automationClient(transport)
        await transport.replaceScene(tracksScene())
        let session = CommandReviewSession(client: client)
        let review = try await session.prepare(.scene(tracksScene()), in: automationZone())
        await transport.replaceScene(replacement)
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes().isEmpty)
    }
}

@Test func nativeTrackScenesKeepTheCatalogUsableAndApplySilentOrPlayingSelectionsOnce() async throws {
    for playing in [false, true] {
        let saved = tracksScene(playing: playing)
        let transport = NativeAutomationTransport(), client = try automationClient(transport)
        await transport.replaceScene(saved)
        await transport.replaceList([automationScene(), saved])
        #expect(try await client.scenes() == [automationScene(), saved])
        #expect(try await client.scene(named: saved.name) == saved)
        let session = CommandReviewSession(client: client)
        let review = try await session.prepare(.scene(saved, fadeSeconds: 4), in: automationZone())
        #expect(review.command.savedScene == saved)
        #expect(await transport.writes().isEmpty)
        let receipt = try await session.execute(review)
        #expect(receipt.title == "Scene applied" && !receipt.partial)
        let writes = await transport.writes()
        let request = try #require(writes.first)
        #expect(writes.count == 1 && request.httpMethod == "POST")
        #expect(request.url?.path.removingPercentEncoding == "/scenes/Ordered evening/apply")
        #expect(try JSONDecoder().decode([String: JSONValue].self, from: #require(request.httpBody)) == ["fade_secs": .integer(4)])
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes().count == 1)
    }
}

@Test func nativeTrackScenesRemainSchedulableAndAChangedSelectionRetiresTheScheduleReview() async throws {
    for changed in [false, true] {
        let saved = tracksScene()
        let transport = ScheduleTestTransport(), client = try scheduleClient(transport)
        await transport.setScene(saved)
        let session = CommandReviewSession(client: client)
        let draft = ScheduleDraft(time: scheduleWeekdays, action: .scene(saved))
        let review = try await session.prepare(ScheduleCommand.add(draft))
        #expect(await transport.writes().isEmpty)
        if changed {
            await transport.setScene(tracksScene(tracks: Array(sceneSelection.reversed())))
            await #expect(throws: ContractError.self) { try await session.execute(review) }
            #expect(await transport.writes().isEmpty)
        } else {
            _ = try await session.execute(review)
            let request = try #require(await transport.writes().first)
            let body = try scheduleBody(request)
            #expect(body["action"] == .string("apply_scene") && body["scene"] == .string(saved.name))
            #expect(body["tracks"] == nil && body["zone"] == nil)
            await #expect(throws: ContractError.self) { try await session.execute(review) }
            #expect(await transport.writes().count == 1)
        }
    }
}

@Test func nativeTrackSceneInvalidDefinitionsAndUncertainWritesNeverSendAnExtraCommand() async throws {
    let transport = NativeAutomationTransport(), client = try automationClient(transport)
    let session = CommandReviewSession(client: client)
    await #expect(throws: ContractError.self) {
        try await session.prepare(.scene(tracksScene(tracks: [])), in: automationZone())
    }
    #expect(await transport.requests.isEmpty)
    await transport.replaceScene(tracksScene())
    let review = try await session.prepare(.scene(tracksScene()), in: automationZone())
    await transport.loseWrite()
    await #expect(throws: ClientError.self) { try await session.execute(review) }
    await #expect(throws: ContractError.self) { try await session.execute(review) }
    #expect(await transport.writes().count == 1)
}

@Test @MainActor func nativeTrackSceneScreenKeepsSelectionsInApplyAndReplacementReviews() async throws {
    let transport = NativeAutomationTransport(), saved = tracksScene()
    await transport.replaceScene(saved)
    await transport.replaceList([automationScene(), saved])
    let model = try await automationModel(transport)
    defer { model.stop() }
    #expect(model.scenes == [automationScene(), saved] && model.panelProblem == nil)
    model.prepare(.scene(saved))
    try await automationEventually { model.review != nil }
    #expect(model.review?.command.savedScene?.groups[0].source.tracks == sceneSelection)
    model.cancelReview()
    model.beginSceneDraft(replacing: saved)
    #expect(model.sceneDraft.groups.isEmpty)
    #expect(model.sceneDraftReplacing == saved)
    #expect(model.sceneDraftReplacing?.groups[0].source.tracks == sceneSelection)
    #expect(await transport.writes().isEmpty)
}
