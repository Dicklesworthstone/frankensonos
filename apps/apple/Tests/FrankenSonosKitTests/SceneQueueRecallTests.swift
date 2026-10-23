import Foundation
import Testing
@testable import FrankenSonosKit

private let queueSceneWire = #"""
{
  "name": "Queue moment", "saved": 1,
  "groups": [{
    "coordinator": "Workshop@TEST-B", "members": ["Hall@TEST-B"], "playing": false,
    "source": {"kind": "queue", "queue": {
      "coordinator": "RINCON_TEST", "household": "TEST-B", "update_id": 42,
      "track": 3, "position_secs": 83, "play_mode": "SHUFFLE_NOREPEAT", "boot_seq": 7
    }}
  }],
  "volumes": {}, "mutes": {}
}
"""#

private let queueBackupSceneWire = queueSceneWire
    .replacingOccurrences(of: #""kind": "queue""#, with: #""kind": "queue_backup""#)
    .replacingOccurrences(of: #""boot_seq": 7"#, with: #""boot_seq": 7, "total": 5"#)

private func queueScene(coordinator: String = "RINCON_TEST", household: String? = "TEST-B",
                        updateId: UInt32 = 42, track: UInt32 = 3, positionSecs: UInt32 = 83,
                        playMode: QueuePlaybackMode? = .shuffleNoRepeat, bootSeq: UInt32? = 7,
                        kind: String = "queue", total: UInt32? = nil) -> Scene {
    Scene(name: "Queue moment", saved: 1,
          groups: [.init(coordinator: "Workshop@TEST-B", members: ["Hall@TEST-B"],
                         source: .init(kind: kind, name: nil, uri: nil, mood: nil,
                                       queue: .init(coordinator: coordinator, household: household,
                                                    updateId: updateId, track: track, positionSecs: positionSecs,
                                                    playMode: playMode, bootSeq: bootSeq, total: total)), playing: false)],
          volumes: [:], mutes: [:])
}

@Test func nativeQueueSceneKeepsCapturedIdentityOccurrenceAndOptionalState() throws {
    let scene = try DaemonClient.decode(Scene.self, from: Data(queueSceneWire.utf8)).validated()
    #expect(scene == queueScene())
    let queue = try #require(scene.groups.first?.source.queue)
    #expect(queue.summary == "Original unchanged queue · track 3 at 1:23")
    #expect(queue.playMode?.summary == "Shuffle · Repeat: off")
    let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
    let encoded = try encoder.encode(scene)
    #expect(try DaemonClient.decode(Scene.self, from: encoded) == scene)
    let json = try #require(JSONSerialization.jsonObject(with: encoded) as? [String: Any])
    let groups = try #require(json["groups"] as? [[String: Any]])
    let source = try #require(groups[0]["source"] as? [String: Any])
    let saved = try #require(source["queue"] as? [String: Any])
    #expect(saved["update_id"] as? Int == 42)
    #expect(saved["position_secs"] as? Int == 83)
    #expect(saved["boot_seq"] as? Int == 7)
    #expect(saved["total"] == nil)
    #expect(queue.recallNote.contains("original unchanged queue"))
}

@Test func nativeQueueBackupKeepsItsCountAndExplainsReplacementAndUndoLimits() throws {
    let scene = try DaemonClient.decode(Scene.self, from: Data(queueBackupSceneWire.utf8)).validated()
    #expect(scene == queueScene(kind: "queue_backup", total: 5))
    let queue = try #require(scene.groups.first?.source.queue)
    #expect(queue.total == 5)
    #expect(queue.summary == "Saved queue backup · 5 entries · track 3 at 1:23")
    #expect(queue.recallNote == "Recall replaces the original player’s current queue. Action-history undo cannot restore the replaced queue contents.")
    let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
    let encoded = try encoder.encode(scene)
    #expect(try DaemonClient.decode(Scene.self, from: encoded).validated() == scene)
    let json = try #require(JSONSerialization.jsonObject(with: encoded) as? [String: Any])
    let groups = try #require(json["groups"] as? [[String: Any]])
    let source = try #require(groups[0]["source"] as? [String: Any])
    let saved = try #require(source["queue"] as? [String: Any])
    #expect(source["kind"] as? String == "queue_backup")
    #expect(saved["total"] as? Int == 5)
    #expect(saved["entries"] == nil)
}

@Test func nativeQueueSceneAllowsAbsentOptionalObservationsAndLegacyKeepSources() throws {
    let minimal = #"{"coordinator":"RINCON_TEST","update_id":0,"track":1,"position_secs":0}"#
    let queue = try DaemonClient.decode(Scene.QueueReference.self, from: Data(minimal.utf8))
    #expect(queue.household == nil && queue.playMode == nil && queue.bootSeq == nil && queue.total == nil)
    let legacy = try DaemonClient.decode(Scene.Source.self, from: Data(#"{"kind":"keep"}"#.utf8))
    #expect(legacy == .init(kind: "keep", name: nil, uri: nil, mood: nil))
    #expect(try queueScene(household: nil, updateId: 0, playMode: nil, bootSeq: nil).validated().groups.count == 1)
}

@Test func nativeQueueSceneRejectsIncompleteOrInvalidQueueReferences() throws {
    for (old, replacement) in [
        (#""track": 3"#, #""track": 0"#),
        (#""update_id": 42"#, #""update_id": -1"#),
        (#""position_secs": 83"#, #""position_secs": 4294967296"#),
        (#""SHUFFLE_NOREPEAT""#, #""UNKNOWN""#),
        (#""coordinator": "RINCON_TEST""#, #""coordinator": """#),
        (#""kind": "queue""#, #""kind": "keep""#),
        (#""queue": {"#, #""missing_queue": {"#)
    ] {
        let wire = queueSceneWire.replacingOccurrences(of: old, with: replacement)
        #expect(throws: (any Error).self) {
            try DaemonClient.decode(Scene.self, from: Data(wire.utf8)).validated()
        }
    }
    for invalid in [queueScene(coordinator: "RINCON_TEST\n"), queueScene(household: ""),
                    queueScene(coordinator: String(repeating: "x", count: 1_025))] {
        #expect(throws: ContractError.self) { try invalid.validated() }
    }
}

@Test func nativeQueueBackupRequiresABoundedCountContainingTheSelectedOccurrence() throws {
    for valid in [queueScene(track: 1, kind: "queue_backup", total: 1),
                  queueScene(track: 1_000, kind: "queue_backup", total: 1_000),
                  queueScene(household: nil, playMode: nil, bootSeq: nil, kind: "queue_backup", total: 5)] {
        #expect(try valid.validated() == valid)
    }
    for invalid in [queueScene(kind: "queue_backup"), queueScene(kind: "queue_backup", total: 0),
                    queueScene(kind: "queue_backup", total: 1_001),
                    queueScene(track: 6, kind: "queue_backup", total: 5),
                    queueScene(track: 0, kind: "queue_backup", total: 5),
                    queueScene(total: 5), queueScene(kind: "keep", total: 5)] {
        #expect(throws: ContractError.self) { try invalid.validated() }
    }
    for total in ["-1", "4294967296", "null"] {
        let wire = queueBackupSceneWire.replacingOccurrences(of: #""total": 5"#, with: "\"total\": \(total)")
        #expect(throws: (any Error).self) {
            try DaemonClient.decode(Scene.self, from: Data(wire.utf8)).validated()
        }
    }
    let missing = queueBackupSceneWire.replacingOccurrences(of: #""queue": {"#, with: #""missing_queue": {"#)
    #expect(throws: ContractError.self) {
        try DaemonClient.decode(Scene.self, from: Data(missing.utf8)).validated()
    }
}

@Test func nativeQueueReferencesCannotBeAuthoredOrHiddenInAnotherSource() throws {
    for scene in [queueScene(), queueScene(kind: "keep"),
                  queueScene(kind: "queue_backup", total: 5), queueScene(kind: "queue_backup")] {
        let definition = SceneDefinition(groups: scene.groups)
        #expect(throws: ContractError.self) {
            try definition.validated(zones: [automationZone(house: "TEST-B")])
        }
    }
}

@Test func nativeQueueSceneCanBeListedReviewedAndAppliedExactlyOnce() async throws {
    for scene in [queueScene(), queueScene(kind: "queue_backup", total: 5)] {
        let transport = NativeAutomationTransport(), client = try automationClient(transport)
        await transport.replaceScene(scene)
        #expect(try await client.scenes() == [scene])
        #expect(try await client.scene(named: scene.name) == scene)
        let session = CommandReviewSession(client: client)
        let review = try await session.prepare(.scene(scene), in: automationZone())
        #expect(await transport.writes().isEmpty)
        _ = try await session.execute(review)
        let writes = await transport.writes()
        #expect(writes.count == 1 && writes[0].url?.path == "/scenes/Queue moment/apply")
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes().count == 1)
    }
}

@Test func nativeQueueBackupCountAndKindChangesInvalidateTheReviewedSceneBeforeWriting() async throws {
    let original = queueScene(kind: "queue_backup", total: 5)
    for changed in [queueScene(kind: "queue_backup", total: 6), queueScene()] {
        let transport = NativeAutomationTransport(), client = try automationClient(transport)
        await transport.replaceScene(original)
        let session = CommandReviewSession(client: client)
        let review = try await session.prepare(.scene(original), in: automationZone())
        await transport.replaceScene(changed)
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes().isEmpty)
    }
}

@Test func nativeQueueReferenceChangesInvalidateTheReviewedSceneBeforeWriting() async throws {
    for changed in [queueScene(coordinator: "RINCON_OTHER"), queueScene(household: "TEST-A"),
                    queueScene(updateId: 43), queueScene(track: 4), queueScene(positionSecs: 84),
                    queueScene(playMode: .normal), queueScene(bootSeq: 8)] {
        let transport = NativeAutomationTransport(), client = try automationClient(transport)
        await transport.replaceScene(queueScene())
        let session = CommandReviewSession(client: client)
        let review = try await session.prepare(.scene(queueScene()), in: automationZone())
        // Keep the name and saved timestamp identical: all reference fields
        // participate in the immutable definition being confirmed.
        await transport.replaceScene(changed)
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes().isEmpty)
    }
}
