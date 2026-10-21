import Foundation
import Testing
@testable import FrankenSonosKit

private let handoffZone = Zone(household: "S2", coordinatorRoom: "Studio", members: ["Studio", "Hall"], transportState: "playing")
private let handoffMood = MoodCatalog.Mood(name: "calm", constraints: ["energy_bias": .integer(-1)])
private func handoffStatus(_ steering: String = #"{"source":"none"}"#) throws -> DJStatus {
    let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
    return try decoder.decode(DJStatus.self, from: Data(#"{"zone":"Studio","running":false,"next":[],"steering":STEERING}"#.replacingOccurrences(of: "STEERING", with: steering).utf8))
}
private func handoffChoices() -> DJRehearsalDraft {
    var draft = DJRehearsalDraft()
    draft.filters[.includeGenres] = "jazz"
    return draft
}
private func handoffDraft(_ choices: DJRehearsalDraft = handoffChoices(),
                          status: DJStatus? = nil, moods: [MoodCatalog.Mood] = []) throws -> DJSteeringDraft {
    try DJSteeringDraft(rehearsal: choices.request(for: handoffZone), in: handoffZone,
                        status: status ?? handoffStatus(), moods: moods)
}

@Test func rehearsalHandoffCopiesEveryExplicitFilterWithoutPlanningMetadata() throws {
    var choices = handoffChoices()
    for field in DJRehearsalDraft.Filter.allCases {
        choices.filters[field] = field == .decades || field == .excludeDecades ? "1960\n1980"
            : field == .periods || field == .excludePeriods ? "Late Romantic\nbaroque" : "Artist, With Comma\nAnother"
    }
    choices.minimumMinutes = "2"; choices.maximumMinutes = "90"
    choices.energyBias = -2; choices.allowLongWorks = true
    choices.length = .hours; choices.hours = 48; choices.start = .houseTime; choices.houseTime = "23:00"
    choices.seed = String(UInt64.max)
    let request = try choices.request(for: handoffZone)
    var draft = try handoffDraft(choices); draft.minutes = "15"
    let change = try draft.change(moods: [])
    #expect(change.filters == (try DJSteeringFilters(request.constraints)))
    #expect(change.filters.values.count == 16)
    let fields = try change.fields(in: handoffZone)
    #expect(Set(fields.keys) == ["zone", "constraints", "for_secs"])
    #expect(fields["zone"] == .string("Studio@S2") && fields["for_secs"] == .integer(900))
    #expect(!change.clearing)
    #expect(try JSONDecoder().decode([String: JSONValue].self, from: JSONEncoder().encode(fields)) == fields)
}

@Test func rehearsalHandoffReplacesRatherThanMergesExistingSessionAdditions() throws {
    let status = try handoffStatus(#"{"source":"session","mood":"old","expires_at":2000000000,"added":{"include_artists":["Old artist"],"exclude_genres":["jazz"],"min_work_minutes":50,"max_work_minutes":60,"energy_bias":2,"allow_long_works":true},"constraints":{"include_artists":["Old artist"]}}"#)
    var draft = try handoffDraft(status: status)
    #expect(draft.entries == [.includeGenres: "jazz"] && draft.mood.isEmpty)
    #expect(draft.minimumMinutes.isEmpty && draft.maximumMinutes.isEmpty)
    #expect(draft.energyBias == 0 && !draft.allowLongWorks)
    draft.minutes = "60"
    let change = try draft.change(moods: [])
    try change.validate(status)
    #expect(throws: ContractError.self) { try change.validate(handoffStatus()) }
    #expect(change.filters.values == ["include_genres": .array([.string("jazz")])])
}

@Test func inheritedRehearsalSteeringCannotBecomeAnImplicitClearOrReplacement() throws {
    let status = try handoffStatus(#"{"source":"program","mood":"calm","constraints":{"energy_bias":-1}}"#)
    #expect(throws: ContractError.self) { try handoffDraft(DJRehearsalDraft(), status: status, moods: [handoffMood]) }
}

@Test func rehearsalMoodOnlyUsesCanonicalCurrentMoodWithoutCopyingInheritedFilters() throws {
    var choices = DJRehearsalDraft(); choices.mood = " CALM "
    var draft = try handoffDraft(choices, moods: [handoffMood])
    #expect(draft.mood == "calm" && draft.entries.isEmpty && draft.energyBias == 0)
    draft.minutes = "1"
    let change = try draft.change(moods: [handoffMood])
    #expect(change.mood == handoffMood && change.filters.values.isEmpty)
    #expect(try change.fields(in: handoffZone) == ["zone": .string("Studio@S2"), "mood": .string("calm"),
        "constraints": .object([:]), "for_secs": .integer(60)])
}

@Test func rehearsalHandoffRequiresAnExplicitNewLifetimeNotPreviewHours() throws {
    var choices = handoffChoices(); choices.length = .hours; choices.hours = 48
    var draft = try handoffDraft(choices)
    #expect(draft.minutes.isEmpty && !draft.untilCleared)
    for value in ["", "0", "10081", "1.5", "1e2", "-1"] {
        draft.minutes = value
        #expect(throws: ContractError.self) { try draft.change(moods: []) }
    }
    for (value, seconds): (String, UInt32) in [("1", 60), ("10080", 604800)] {
        draft.minutes = value
        #expect(try draft.change(moods: []).seconds == seconds)
    }
    draft.minutes = ""; draft.untilCleared = true
    #expect(try draft.change(moods: []).fields(in: handoffZone)["for_secs"] == nil)
}

@Test func rehearsalSettingsOutsideLiveSteeringLimitsAreNeverTruncatedOrDiscarded() throws {
    var tooMany = handoffChoices(); tooMany.filters[.includeArtists] = Array(repeating: "Artist", count: 33).joined(separator: "\n")
    var tooLong = handoffChoices(); tooLong.filters[.includeArtists] = String(repeating: "a", count: 129)
    var zero = handoffChoices(); zero.minimumMinutes = "0"
    var length = handoffChoices(); length.maximumMinutes = "601"
    var decade = handoffChoices(); decade.filters[.decades] = "990"
    var mood = handoffChoices(); mood.mood = String(repeating: "a", count: 129)
    for choices in [tooMany, tooLong, zero, length, decade, mood] {
        _ = try choices.request(for: handoffZone) // accepted by the existing preview draft
        #expect(throws: ContractError.self) { try handoffDraft(choices) }
    }
}

@Test func rehearsalSettingsAtLiveBoundariesRemainExactAndOrdered() throws {
    var choices = handoffChoices()
    let name = String(repeating: "e\u{301}", count: 64)
    choices.filters[.includeArtists] = ([name, "Earth, Wind & Fire"] + Array(repeating: "Other", count: 30)).joined(separator: "\n")
    choices.minimumMinutes = "1"; choices.maximumMinutes = "600"
    var draft = try handoffDraft(choices); draft.minutes = "1"
    let change = try draft.change(moods: [])
    #expect(change.filters == (try DJSteeringFilters(choices.request(for: handoffZone).constraints)))
    #expect(draft.entries[.includeArtists] == choices.filters[.includeArtists])
}

@Test func rehearsalHandoffRejectsMissingAmbiguousAndInvalidCurrentMoodDefinitions() throws {
    var choices = handoffChoices(); choices.mood = "calm"
    for moods in [[], [handoffMood, handoffMood],
                  [MoodCatalog.Mood(name: "calm", constraints: ["future_filter": .bool(true)])]] {
        #expect(throws: ContractError.self) { try handoffDraft(choices, moods: moods) }
    }
    var draft = try handoffDraft(choices, moods: [handoffMood]); draft.minutes = "60"
    let change = try draft.change(moods: [handoffMood])
    #expect(throws: ContractError.self) {
        try change.validateMoods([MoodCatalog.Mood(name: "calm", constraints: ["energy_bias": .integer(2)])])
    }
}

@Test func rehearsalHandoffDoesNotFuzzilyMatchUnicodeMoodNames() throws {
    var choices = handoffChoices(); choices.mood = "cafe\u{301}"
    #expect(throws: ContractError.self) {
        try handoffDraft(choices, moods: [MoodCatalog.Mood(name: "café", constraints: [:])])
    }
}

@Test func rehearsalHandoffRejectsDifferentHouseholdsMembershipAndCoordinator() throws {
    let request = try handoffChoices().request(for: handoffZone)
    for zone in [Zone(household: "S1", coordinatorRoom: "Studio", members: ["Studio", "Hall"], transportState: "playing"),
                 Zone(household: "S2", coordinatorRoom: "Studio", members: ["Studio"], transportState: "playing"),
                 Zone(household: "S2", coordinatorRoom: "Hall", members: ["Studio", "Hall"], transportState: "playing")] {
        #expect(throws: ContractError.self) { try DJSteeringDraft(rehearsal: request, in: zone, status: handoffStatus(), moods: []) }
    }
}

@Test func rehearsalHandoffAllowsSameGroupOrderAndPlaybackProgress() throws {
    let request = try handoffChoices().request(for: handoffZone)
    let reordered = Zone(household: "S2", coordinatorRoom: "Studio", members: ["Hall", "Studio"], transportState: "paused")
    var draft = try DJSteeringDraft(rehearsal: request, in: reordered, status: handoffStatus(), moods: [])
    draft.minutes = "60"
    #expect(try draft.change(moods: []).fields(in: handoffZone)["zone"] == .string("Studio@S2"))
}

@Test func rehearsalHandoffCannotOverwriteUninterpretableLiveSteering() throws {
    #expect(throws: ContractError.self) { try handoffDraft(status: handoffStatus(#"{"source":"future"}"#)) }
    #expect(throws: ContractError.self) { try handoffDraft(status: handoffStatus(#"{"source":"session","added":{"new_filter":true}}"#)) }
}

@Test func rehearsalHandoffKeepsTheSeparateEditorAndReviewedChangeIndependent() throws {
    var existing = try DJSteeringDraft(in: handoffZone, status: handoffStatus())
    existing.entries[.includeArtists] = "Unfinished editor choice"; existing.minutes = "120"
    var imported = try handoffDraft(); imported.minutes = "30"
    let change = try imported.change(moods: [])
    imported.entries[.includeGenres] = "rock"; imported.minutes = "45"
    #expect(existing.entries[.includeArtists] == "Unfinished editor choice" && existing.minutes == "120")
    #expect(change.seconds == 1800 && change.filters.values == ["include_genres": .array([.string("jazz")])])
}

private func handoffReport(_ request: DJRehearsalRequest) -> DJRehearsal {
    DJRehearsal(zone: request.zone.coordinatorRoom, from: "2030-01-01T23:00:00Z", seed: request.seed, works: [],
                summary: .init(works: 0, minutes: 0, distinctArtists: 0, explicit: 0, hours: []))
}
@MainActor private func handoffEventually(_ condition: () -> Bool) async throws {
    let clock = ContinuousClock(), end = clock.now + .seconds(3)
    while !condition() && clock.now < end { try await Task.sleep(for: .milliseconds(2)) }
    #expect(condition())
    if !condition() { throw ContractError.invalid("Handoff test timed out") }
}
@MainActor private func handoffModel() async throws -> DJRehearsalModel {
    let model = DJRehearsalModel(read: { handoffReport($0) }, now: { Date(timeIntervalSince1970: 1) })
    model.activate(in: handoffZone); model.update(handoffChoices()); model.generate()
    try await handoffEventually { model.matchesChoices && !model.loading }
    return model
}

@Test @MainActor func completedRehearsalMakesALocalDraftWithoutChangingPreviewOrStartingAnotherRead() async throws {
    let model = try await handoffModel()
    defer { model.stop() }
    let observation = try #require(model.observation), original = model.draft
    var draft = try model.steeringDraft(from: observation.id, in: handoffZone, status: handoffStatus(), moods: [])
    draft.minutes = "60"
    #expect(try draft.change(moods: []).filters.values == observation.request.constraints)
    #expect(model.observation?.id == observation.id && model.draft == original && !model.loading)
    #expect(model.matchesChoices)
}

@Test @MainActor func rehearsalHandoffRejectsUnissuedAndObsoleteObservationIdentities() async throws {
    let model = try await handoffModel(); defer { model.stop() }
    let before = try #require(model.observation)
    #expect(throws: ContractError.self) { try model.steeringDraft(from: UUID(), in: handoffZone, status: handoffStatus(), moods: []) }
    model.generate()
    #expect(throws: ContractError.self) { try model.steeringDraft(from: before.id, in: handoffZone, status: handoffStatus(), moods: []) }
    try await handoffEventually { model.matchesChoices && !model.loading }
    let after = try #require(model.observation)
    #expect(before.generatedAt == after.generatedAt && before.request == after.request)
    #expect(before.id != after.id)
    #expect(throws: ContractError.self) { try model.steeringDraft(from: before.id, in: handoffZone, status: handoffStatus(), moods: []) }
}

@Test @MainActor func editingAndRevertingPreviewChoicesStillRequiresRegenerationForHandoff() async throws {
    let model = try await handoffModel(); defer { model.stop() }
    let observation = try #require(model.observation), original = model.draft
    var changed = original; changed.seed = "2"
    model.update(changed); model.update(original)
    #expect(model.observation?.id == observation.id && !model.matchesChoices)
    #expect(throws: ContractError.self) { try model.steeringDraft(from: observation.id, in: handoffZone, status: handoffStatus(), moods: []) }
}

@Test @MainActor func suspendedCancelledAndStoppedPreviewsCannotBecomeLiveSettings() async throws {
    for action in 0..<3 {
        let model = try await handoffModel(), observation = try #require(model.observation)
        if action == 0 { model.suspend(); model.activate(in: handoffZone) }
        else if action == 1 { model.cancel() }
        else { model.stop(); model.activate(in: handoffZone) }
        #expect(throws: ContractError.self) { try model.steeringDraft(from: observation.id, in: handoffZone, status: handoffStatus(), moods: []) }
        model.stop()
    }
}

@Test @MainActor func changingSelectedGroupsCannotRetargetAPreviewHandoff() async throws {
    let model = try await handoffModel(); defer { model.stop() }
    let observation = try #require(model.observation)
    let other = Zone(household: "S1", coordinatorRoom: "Studio", members: ["Studio", "Hall"], transportState: "playing")
    #expect(throws: ContractError.self) { try model.steeringDraft(from: observation.id, in: other, status: handoffStatus(), moods: []) }
    model.activate(in: other)
    #expect(model.observation == nil)
    #expect(throws: ContractError.self) { try model.steeringDraft(from: observation.id, in: handoffZone, status: handoffStatus(), moods: []) }
}

@Test @MainActor func aPreviewWithNoOverridesCannotApplyTheCurrentlyObservedLiveSession() async throws {
    let model = DJRehearsalModel(read: { handoffReport($0) }); defer { model.stop() }
    model.activate(in: handoffZone); model.generate()
    try await handoffEventually { model.matchesChoices && !model.loading }
    let observation = try #require(model.observation)
    let current = try handoffStatus(#"{"source":"session","mood":"calm","added":{"energy_bias":-1}}"#)
    #expect(throws: ContractError.self) { try model.steeringDraft(from: observation.id, in: handoffZone, status: current, moods: [handoffMood]) }
}
