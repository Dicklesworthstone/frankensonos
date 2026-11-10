import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

private let savedPlaylistReference = Scene.PlaylistReference(
    household: "TEST-B", id: "SQ:7", title: "Café evenings"
)

private let savedPlaylistSceneWire = #"""
{
  "name": "Playlist evening", "saved": 1,
  "groups": [{
    "coordinator": "Workshop@TEST-B", "members": ["Hall@TEST-B"], "playing": false,
    "source": {
      "kind": "sonos_playlist", "name": "SQ:7",
      "playlist": {"household": "TEST-B", "id": "SQ:7", "title": "Café evenings"}
    }
  }],
  "volumes": {}, "mutes": {}
}
"""#

private func savedPlaylistScene(reference: Scene.PlaylistReference = savedPlaylistReference,
                                playing: Bool? = false, source: Scene.Source? = nil) -> Scene {
    Scene(name: "Playlist evening", saved: 1,
          groups: [.init(coordinator: "Workshop@TEST-B", members: ["Hall@TEST-B"],
                         source: source ?? .init(kind: "sonos_playlist", name: reference.id,
                                                 uri: nil, mood: nil, playlist: reference),
                         playing: playing)],
          volumes: [:], mutes: [:])
}

@Test func nativeSonosPlaylistSceneDecodesTheDaemonContractAndRetainsItsPinnedIdentity() throws {
    let scene = try DaemonClient.decode(Scene.self, from: Data(savedPlaylistSceneWire.utf8)).validated()
    #expect(scene == savedPlaylistScene())
    let reference = try #require(scene.groups.first?.source.playlist)
    #expect(reference == savedPlaylistReference)
    #expect(reference.summary == "Saved Sonos playlist: Café evenings · SQ:7 · TEST-B")
    #expect(reference.recallNote.contains("current contents"))
    #expect(reference.recallNote.contains("replaces the group’s queue"))
    #expect(reference.recallNote.contains("renamed or missing"))
    #expect(reference.recallNote.contains("undo cannot restore"))

    let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
    let encoded = try encoder.encode(scene)
    #expect(try DaemonClient.decode(Scene.self, from: encoded).validated() == scene)
    let json = try #require(JSONSerialization.jsonObject(with: encoded) as? [String: Any])
    let groups = try #require(json["groups"] as? [[String: Any]])
    let source = try #require(groups[0]["source"] as? [String: Any])
    #expect(source["kind"] as? String == "sonos_playlist")
    #expect(source["name"] as? String == "SQ:7")
    #expect(source["playlist"] as? [String: String] ==
            ["household": "TEST-B", "id": "SQ:7", "title": "Café evenings"])
    #expect(groups[0]["playing"] as? Bool == false)
    #expect(source["queue"] == nil && source["uri"] == nil)

    let legacy = try DaemonClient.decode(Scene.Source.self, from: Data(#"{"kind":"keep"}"#.utf8))
    #expect(legacy.playlist == nil)
    #expect(legacy == .init(kind: "keep", name: nil, uri: nil, mood: nil))
}

@Test func nativeSonosPlaylistSceneValidatesNativeIdsAndBoundsInUTF8Bytes() throws {
    let valid: [Scene.PlaylistReference] = [
        .init(household: "TEST-B", id: "SQ:0", title: "One"),
        .init(household: "TEST-B", id: "SQ:7", title: "Family 👩‍👩‍👧‍👦"),
        .init(household: "TEST-B", id: "SQ:7", title: " \n" + String(repeating: "x", count: 1_023) + "\t "),
        .init(household: String(repeating: "h", count: 1_024),
              id: "SQ:" + String(repeating: "7", count: 1_021),
              title: String(repeating: "é", count: 511) + "a")
    ]
    for reference in valid {
        #expect(try savedPlaylistScene(reference: reference).validated().groups[0].source.playlist == reference)
    }
    for id in ["", "SQ:", "SQ:-1", "SQ:7/8", "SQ:７", "sq:7", "SQ:7\n",
               "SQ:" + String(repeating: "7", count: 1_022)] {
        let reference = Scene.PlaylistReference(household: "TEST-B", id: id, title: "Evening")
        #expect(throws: ContractError.self) { try savedPlaylistScene(reference: reference).validated() }
    }
    for household in ["", " \t ", "TEST-B\n", String(repeating: "h", count: 1_025)] {
        let reference = Scene.PlaylistReference(household: household, id: "SQ:7", title: "Evening")
        #expect(throws: ContractError.self) { try savedPlaylistScene(reference: reference).validated() }
    }
    for title in ["", " \t ", "Even\ning", "Even\u{7f}ing", String(repeating: "é", count: 512)] {
        let reference = Scene.PlaylistReference(household: "TEST-B", id: "SQ:7", title: title)
        #expect(throws: ContractError.self) { try savedPlaylistScene(reference: reference).validated() }
    }
}

@Test func nativeSonosPlaylistSceneRejectsMissingMismatchedAndMixedSourceIdentity() throws {
    let reference = savedPlaylistReference
    let invalid: [Scene.Source] = [
        .init(kind: "sonos_playlist", name: "SQ:7", uri: nil, mood: nil),
        .init(kind: "sonos_playlist", name: nil, uri: nil, mood: nil, playlist: reference),
        .init(kind: "sonos_playlist", name: "SQ:8", uri: nil, mood: nil, playlist: reference),
        .init(kind: "sonos_playlist", name: "SQ:7", uri: "test:source", mood: nil, playlist: reference),
        .init(kind: "sonos_playlist", name: "SQ:7", uri: nil, mood: "calm", playlist: reference),
        .init(kind: "sonos_playlist", name: "SQ:7", uri: nil, mood: nil,
              queue: .init(coordinator: "RINCON_TEST", household: "TEST-B", updateId: 1, track: 1,
                           positionSecs: 0, playMode: nil, bootSeq: nil, total: nil), playlist: reference),
        .init(kind: "keep", name: nil, uri: nil, mood: nil, playlist: reference),
        .init(kind: "favorite", name: "Evening", uri: nil, mood: nil, playlist: reference)
    ]
    for source in invalid {
        #expect(throws: ContractError.self) { try savedPlaylistScene(source: source).validated() }
    }
    for field in ["household", "id", "title"] {
        let wire = savedPlaylistSceneWire.replacingOccurrences(of: "\"\(field)\":", with: "\"missing_\(field)\":")
        #expect(throws: (any Error).self) {
            try DaemonClient.decode(Scene.self, from: Data(wire.utf8)).validated()
        }
    }
}

@Test func nativeSonosPlaylistIdentityComparisonKeepsDifferentUnicodeBytesDistinct() {
    let decomposed = Scene.PlaylistReference(household: "TEST-B", id: "SQ:7", title: "Cafe\u{301} evenings")
    // Swift String equality alone merges these spellings; the daemon pins the
    // exact catalog title, so a review must retain the wire bytes.
    #expect(decomposed.title == savedPlaylistReference.title)
    #expect(decomposed != savedPlaylistReference)
    #expect(savedPlaylistScene(reference: decomposed) != savedPlaylistScene())
}

@Test func nativeSonosPlaylistReadOnlyReferencesCannotBeCopiedIntoCustomDefinitions() throws {
    for source in [
        Scene.Source(kind: "sonos_playlist", name: "SQ:7", uri: nil, mood: nil, playlist: savedPlaylistReference),
        Scene.Source(kind: "sonos_playlist", name: "SQ:7", uri: nil, mood: nil),
        Scene.Source(kind: "keep", name: nil, uri: nil, mood: nil, playlist: savedPlaylistReference)
    ] {
        let definition = SceneDefinition(groups: savedPlaylistScene(source: source).groups)
        #expect(throws: ContractError.self) {
            try definition.validated(zones: [automationZone(house: "TEST-B")])
        }
    }
}

@Test func nativeSonosPlaylistScenesListReviewAndApplyOnceWithSilentAndPlayingDefinitions() async throws {
    let cases: [(Bool, UInt32?)] = [(false, nil), (true, 4)]
    for (playing, fade) in cases {
        let scene = savedPlaylistScene(playing: playing)
        let transport = NativeAutomationTransport(), client = try automationClient(transport)
        await transport.replaceScene(scene)
        await transport.replaceList([automationScene(), scene])
        #expect(try await client.scenes() == [automationScene(), scene])
        #expect(try await client.scene(named: scene.name) == scene)

        let session = CommandReviewSession(client: client)
        let review = try await session.prepare(.scene(scene, fadeSeconds: fade), in: automationZone())
        #expect(review.command == .scene(scene, fadeSeconds: fade))
        #expect(await transport.writes().isEmpty)
        let receipt = try await session.execute(review)
        #expect(receipt.title == "Scene applied" && !receipt.partial)
        let writes = await transport.writes()
        let request = try #require(writes.first)
        #expect(writes.count == 1 && request.httpMethod == "POST")
        #expect(request.url?.path.removingPercentEncoding == "/scenes/Playlist evening/apply")
        // The daemon recalls the retained definition. The initiating household
        // is not a filter, and no read-only playlist reference is republished.
        #expect(request.url?.query == nil)
        var expected: [String: JSONValue] = [:]
        if let fade { expected["fade_secs"] = .integer(Int64(fade)) }
        #expect(try JSONDecoder().decode([String: JSONValue].self,
                                        from: #require(request.httpBody)) == expected)
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes().count == 1)
    }
}

@Test func nativeSonosPlaylistDefinitionChangesInvalidateReviewBeforeApplying() async throws {
    let changed: [Scene] = [
        savedPlaylistScene(reference: .init(household: "TEST-A", id: "SQ:7", title: "Café evenings")),
        savedPlaylistScene(reference: .init(household: "TEST-B", id: "SQ:8", title: "Café evenings")),
        savedPlaylistScene(reference: .init(household: "TEST-B", id: "SQ:7", title: "Other evening")),
        savedPlaylistScene(reference: .init(household: "TEST-B", id: "SQ:7", title: "Cafe\u{301} evenings")),
        savedPlaylistScene(playing: true),
        savedPlaylistScene(source: .init(kind: "favorite", name: "Evening", uri: nil, mood: nil))
    ]
    for scene in changed {
        let transport = NativeAutomationTransport(), client = try automationClient(transport)
        await transport.replaceScene(savedPlaylistScene())
        let session = CommandReviewSession(client: client)
        let review = try await session.prepare(.scene(savedPlaylistScene()), in: automationZone())
        // Both the scene name and saved timestamp remain identical. Every
        // pinned identity field and playback intent still invalidates review.
        await transport.replaceScene(scene)
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes().isEmpty)
    }
}

@Test func nativeSonosPlaylistMalformedDefinitionFailsBeforeAnyDaemonReadOrWrite() async throws {
    let transport = NativeAutomationTransport(), client = try automationClient(transport)
    let session = CommandReviewSession(client: client)
    let scene = savedPlaylistScene(source: .init(kind: "sonos_playlist", name: "SQ:7", uri: nil, mood: nil))
    await #expect(throws: ContractError.self) { try await session.prepare(.scene(scene), in: automationZone()) }
    #expect(await transport.requests.isEmpty)
}

@Test func nativeSonosPlaylistRecallRetainsPartialOutcomesAndNeverRetriesUncertainWrites() async throws {
    for outcome in ["partial", "lost", "malformed"] {
        let transport = NativeAutomationTransport(), client = try automationClient(transport)
        await transport.replaceScene(savedPlaylistScene())
        let session = CommandReviewSession(client: client)
        let review = try await session.prepare(.scene(savedPlaylistScene()), in: automationZone())
        if outcome == "partial" {
            await transport.partialResult()
            let receipt = try await session.execute(review)
            #expect(receipt.partial)
            for detail in ["Set level", "Speaker unavailable", "Missing room", "Capped at 25"] {
                #expect(receipt.detail.contains(detail))
            }
        } else {
            if outcome == "lost" { await transport.loseWrite() } else { await transport.breakWrite() }
            await #expect(throws: ClientError.self) { try await session.execute(review) }
        }
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes().count == 1)
    }
}

@Test func nativeSonosPlaylistSceneCanBeReviewedForDeletionWithoutReadingSpeakers() async throws {
    let transport = NativeAutomationTransport(), client = try automationClient(transport)
    let scene = savedPlaylistScene()
    await transport.replaceScene(scene)
    let session = CommandReviewSession(client: client)
    let review = try await session.prepare(SceneMutation.delete(scene))
    #expect(review.command.previous == scene)
    #expect(await transport.requests.allSatisfy { $0.url?.path.hasPrefix("/scenes") == true })
    #expect(await transport.writes().isEmpty)
}

@Test @MainActor func nativeSonosPlaylistSceneKeepsTheWholeNativeSceneCatalogUsable() async throws {
    let transport = NativeAutomationTransport()
    let scene = savedPlaylistScene()
    await transport.replaceScene(scene)
    await transport.replaceList([automationScene(), scene])
    let model = try await automationModel(transport)
    defer { model.stop() }
    #expect(model.scenes == [automationScene(), scene])
    #expect(model.panelProblem == nil)
    model.prepare(.scene(scene))
    try await automationEventually { model.review != nil }
    #expect(model.review?.command.savedScene == scene)
    #expect(await transport.writes().isEmpty)
}
