import Foundation
import Testing
@testable import FrankenSonosKit

private let savedFilters: [String: JSONValue] = [
    "include_composers": .array([.string("Bach")]), "exclude_composers": .array([.string("Wagner")]),
    "include_artists": .array([.string("Nina Simone")]), "exclude_artists": .array([.string("Other artist")]),
    "include_genres": .array([.string("jazz")]), "exclude_genres": .array([.string("metal")]),
    "periods": .array([.string("baroque")]), "exclude_periods": .array([.string("late_romantic")]),
    "decades": .array([.integer(1960)]), "exclude_decades": .array([.integer(1980)]),
    "include_keywords": .array([.string("piano")]), "exclude_keywords": .array([.string("opera")]),
    "min_work_minutes": .integer(2), "max_work_minutes": .integer(45),
    "energy_bias": .integer(-1), "allow_long_works": .bool(true)
]

private func savedDirection(_ filters: [String: JSONValue] = savedFilters, seconds: UInt32? = 27) -> DJDirection {
    DJDirection(constraints: filters, forSecs: seconds)
}

private func directedScene(_ direction: DJDirection? = savedDirection(), kind: String = "dj", mood: String? = "calm") -> Scene {
    Scene(name: "Piano evening", saved: 1,
          groups: [.init(coordinator: "Workshop@TEST-A", members: ["Hall@TEST-A"],
                         source: .init(kind: kind, name: nil, uri: nil, mood: mood, direction: direction), playing: true)],
          volumes: [:], mutes: [:])
}

private func directedSchedule(_ direction: DJDirection? = savedDirection()) throws -> SavedSchedule {
    let patch = direction.map { ["direction": JSONValue.object($0.fields)] } ?? [:]
    return try scheduleDecode(scheduleFixture(patch)).validated()
}

private func changedDirections() -> [DJDirection?] {
    var changes: [DJDirection?] = [nil, DJDirection(), savedDirection(seconds: 28), savedDirection(seconds: nil)]
    for field in savedFilters.keys.sorted() {
        var changed = savedFilters
        changed.removeValue(forKey: field)
        changes.append(savedDirection(changed))
    }
    return changes
}

@Test func savedDJDirectionDecodesEveryFieldAndRetainsExactWireValues() throws {
    let expected = savedDirection()
    let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
    let data = try encoder.encode(expected)
    let wire = try JSONDecoder().decode([String: JSONValue].self, from: data)
    #expect(wire == ["constraints": .object(savedFilters), "for_secs": .integer(27)])
    #expect(try DaemonClient.decode(DJDirection.self, from: data) == expected)
    #expect(try expected.validated(mood: "calm") == expected)
    let summary = expected.summary(mood: "calm")
    for text in ["Nina Simone", "Bach", "Wagner", "jazz", "metal", "late_romantic", "1960", "1980", "piano", "opera", "45", "Energy shift: -1", "Allow long works", "27 seconds from each start"] {
        #expect(summary.contains(text))
    }
    let scene = directedScene()
    #expect(try DaemonClient.decode(Scene.self, from: encoder.encode(scene)).validated() == scene)
    #expect(scene.groups[0].source.djSummary == summary)
}

@Test func savedDJDirectionPreservesLegacyAbsenceAndExplicitAutomatic() throws {
    let old = try DaemonClient.decode(Scene.Source.self, from: Data(#"{"kind":"dj"}"#.utf8))
    let automatic = try DaemonClient.decode(Scene.Source.self, from: Data(#"{"kind":"dj","direction":{}}"#.utf8))
    #expect(old.direction == nil && automatic.direction == DJDirection())
    #expect(old != automatic)
    #expect(old.djSummary.contains("existing direction"))
    #expect(automatic.djSummary.contains("clear temporary steering"))
    #expect(try directedScene(DJDirection(), mood: nil).validated().groups[0].source == automatic)
    #expect(try directedSchedule().direction == savedDirection())
    #expect(try directedSchedule(nil).direction == nil)
    #expect(try !directedSchedule(nil).sameDefinition(as: directedSchedule(DJDirection())))
    #expect(try savedDirection(seconds: 1).validated(mood: "calm").forSecs == 1)
    #expect(try savedDirection(seconds: 604_800).validated(mood: "calm").forSecs == 604_800)
}

@Test func savedDJDirectionRejectsMalformedFieldsAndContradictoryActions() throws {
    for wire in [
        #"{"constraints":null}"#, #"{"constraints":[]}"#,
        #"{"constraints":{"include_genre":["jazz"]}}"#,
        #"{"constraints":{"energy_bias":3}}"#,
        #"{"constraints":{"min_work_minutes":50,"max_work_minutes":20}}"#,
        #"{"constraints":{"periods":["unknown"]}}"#,
        #"{"for_secs":0}"#, #"{"for_secs":604801}"#, #"{"for_secs":-1}"#,
        #"{"for_secs":1.5}"#, #"{"for_secs":"27"}"#, #"{"expires_at":2000000000}"#
    ] {
        #expect(throws: (any Error).self) { try DaemonClient.decode(DJDirection.self, from: Data(wire.utf8)) }
    }
    #expect(throws: ContractError.self) { try DJDirection(forSecs: 27).validated(mood: nil) }
    #expect(throws: ContractError.self) { try directedScene(kind: "keep").validated() }
    #expect(throws: ContractError.self) {
        try scheduleDecode(scheduleFixture(["action": .string("pause"), "direction": .object(savedDirection().fields)], removing: ["mood"])).validated()
    }
    #expect(throws: ContractError.self) {
        try SceneDefinition(groups: directedScene(kind: "keep").groups).validated(zones: [automationZone()])
    }
}

@Test func nativeDirectedSceneDefinitionKeepsFiltersThroughAuthoringValidation() throws {
    let source = directedScene().groups[0].source
    let checked = try SceneDefinition(groups: directedScene().groups).validated(zones: [automationZone()])
    #expect(checked.groups[0].source == source)
    let automatic = directedScene(DJDirection(), mood: nil)
    #expect(try SceneDefinition(groups: automatic.groups).validated(zones: [automationZone()]).groups == automatic.groups)
}

@Test func nativeDirectionDefaultsNormalizeLikeTheDaemonBeforeComparingAnAuthoredScene() throws {
    let requested = DJDirection(constraints: ["include_artists": .array([.string("Miles Davis")])])
    let returned = try DaemonClient.decode(DJDirection.self, from: Data(#"{"constraints":{"include_artists":["Miles Davis"],"energy_bias":0}}"#.utf8))
    #expect(requested == returned)
    let withDefaults = DJDirection(constraints: ["include_artists": .array([.string("Miles Davis")]),
        "energy_bias": .integer(0), "allow_long_works": .bool(false), "exclude_artists": .array([])])
    #expect(try withDefaults.validated(mood: nil) == requested)
    let scene = directedScene(withDefaults, mood: nil)
    let checked = try SceneDefinition(groups: scene.groups).validated(zones: [automationZone()])
    #expect(checked.groups[0].source.direction == returned)
    #expect(try DaemonClient.decode(DJDirection.self, from: Data(#"{"constraints":{"energy_bias":0,"allow_long_works":false}}"#.utf8)) == DJDirection())
    #expect(requested != DJDirection())
}

@Test func nativeScheduleReadsLongCompleteDirectionSummariesWithinABoundedResponse() throws {
    let summary = String(repeating: "Artist and genre details; ", count: 400)
    let value = try scheduleDecode(scheduleFixture(["summary": .string(summary), "direction": .object(savedDirection().fields)])).validated()
    #expect(value.summary == summary && value.direction == savedDirection())
    #expect(throws: ContractError.self) {
        try scheduleDecode(scheduleFixture(["summary": .string(String(repeating: "x", count: 262_145))])).validated()
    }
}

@Test func nativeDirectedSceneCanBeReadReviewedAndAppliedOnce() async throws {
    let transport = NativeAutomationTransport(), client = try automationClient(transport)
    let scene = directedScene()
    await transport.replaceScene(scene)
    #expect(try await client.scenes() == [scene])
    let session = CommandReviewSession(client: client)
    let review = try await session.prepare(.scene(scene), in: automationZone())
    #expect(await transport.writes().isEmpty)
    _ = try await session.execute(review)
    #expect(await transport.writes().count == 1)
    await #expect(throws: ContractError.self) { try await session.execute(review) }
    #expect(await transport.writes().count == 1)
}

@Test func everySavedDirectionFieldParticipatesInSceneReview() async throws {
    for direction in changedDirections() {
        let transport = NativeAutomationTransport(), client = try automationClient(transport)
        await transport.replaceScene(directedScene())
        let session = CommandReviewSession(client: client)
        let review = try await session.prepare(.scene(directedScene()), in: automationZone())
        await transport.replaceScene(directedScene(direction))
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes().isEmpty)
    }
}

@Test func everySavedDirectionFieldParticipatesInScheduleReview() async throws {
    let expected = try directedSchedule()
    #expect(ScheduleCommand.change(expected, .pause).detail.contains("Nina Simone"))
    #expect(ScheduleCommand.change(expected, .pause).detail.contains("27 seconds from each start"))
    for direction in changedDirections() {
        let current = try directedSchedule(direction)
        #expect(!expected.sameDefinition(as: current))
        let transport = ScheduleTestTransport(), client = try scheduleClient(transport)
        await transport.setRows([scheduleFixture(["direction": .object(savedDirection().fields)])])
        let session = CommandReviewSession(client: client)
        let review = try await session.prepare(ScheduleCommand.change(expected, .pause))
        let patch = direction.map { ["direction": JSONValue.object($0.fields)] } ?? [:]
        await transport.setRows([scheduleFixture(patch)])
        await #expect(throws: ContractError.self) { try await session.execute(review) }
        #expect(await transport.writes().isEmpty)
    }
}

@Test func nativeDirectedScheduleCanBePausedWithItsDefinitionIntact() async throws {
    let transport = ScheduleTestTransport(), client = try scheduleClient(transport)
    await transport.setRows([scheduleFixture(["direction": .object(savedDirection().fields)])])
    let expected = try #require(try await client.schedules().first)
    #expect(expected.direction == savedDirection())
    let session = CommandReviewSession(client: client)
    let review = try await session.prepare(ScheduleCommand.change(expected, .pause))
    let receipt = try await session.execute(review)
    #expect(receipt.schedule.direction == savedDirection() && !receipt.schedule.enabled)
    #expect(receipt.detail.contains("Nina Simone") && receipt.detail.contains("27 seconds from each start"))
    #expect(await transport.writes().count == 1)
}
