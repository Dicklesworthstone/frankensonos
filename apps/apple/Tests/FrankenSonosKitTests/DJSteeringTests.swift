import Foundation
import Testing
@testable import FrankenSonosKit

private let steeringZone = Zone(household: "S2", coordinatorRoom: "Studio", members: ["Studio"], transportState: "playing")
private func steerStatus(_ steering: String = #"{"source":"none"}"#) throws -> DJStatus {
    let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
    return try decoder.decode(DJStatus.self, from: Data(#"{"zone":"Studio","running":false,"steering":STEERING}"#.replacingOccurrences(of: "STEERING", with: steering).utf8))
}
private func makeChange(_ fields: [String: JSONValue] = ["include_genres": .array([.string("jazz")])],
                        seconds: UInt32? = 3600) throws -> DJSteeringChange {
    try DJSteeringChange(in: steeringZone, before: steerStatus().steering, mood: nil,
                         filters: DJSteeringFilters(fields), seconds: seconds)
}

@Test func allTemporarySteeringFieldsHaveCanonicalWireValues() throws {
    var fields: [String: JSONValue] = [:]
    for field in DJSteeringField.allCases {
        fields[field.rawValue] = .array([field.isDecade ? .integer(1980) : .string(field.isPeriod ? "Late Romantic" : " Example ")])
    }
    fields["min_work_minutes"] = .integer(5); fields["max_work_minutes"] = .integer(90)
    fields["energy_bias"] = .integer(-2); fields["allow_long_works"] = .bool(true)
    let filters = try DJSteeringFilters(fields)
    #expect(filters.values.count == 16)
    #expect(filters.values["include_artists"] == .array([.string("Example")]))
    #expect(filters.values["periods"] == .array([.string("late_romantic")]))
    #expect(filters.values["decades"] == .array([.integer(1980)]))
    #expect(filters.summary.contains("Minimum work minutes: 5"))
    #expect(filters.summary.contains("Exclude release decades: 1980"))
}

@Test func temporaryFilterDefaultsAreSparseButMalformedValuesFail() throws {
    #expect(try DJSteeringFilters(["energy_bias": .integer(0), "allow_long_works": .bool(false),
        "periods": .array([]), "min_work_minutes": .null]).values.isEmpty)
    for fields: [String: JSONValue] in [
        ["future_filter": .bool(true)], ["include_artists": .string("Artist")],
        ["include_genres": .array([.integer(1)])], ["periods": .array([.string("future")])],
        ["decades": .array([.integer(1985)])], ["decades": .array([.string("1980")])],
        ["decades": .array([.integer(990)])], ["decades": .array([.integer(65540)])],
        ["energy_bias": .integer(3)], ["energy_bias": .number(1.5)],
        ["allow_long_works": .string("false")], ["min_work_minutes": .integer(0)],
        ["max_work_minutes": .integer(601)], ["min_work_minutes": .integer(30), "max_work_minutes": .integer(29)]
    ] { #expect(throws: ContractError.self) { try DJSteeringFilters(fields) } }
}

@Test func temporaryFilterLimitsCountScalarsAndNeverSplitCommas() throws {
    #expect(try DJSteeringFilters(["include_artists": .array([.string("Earth, Wind & Fire")])]).values["include_artists"] == .array([.string("Earth, Wind & Fire")]))
    #expect(try DJSteeringFilters(["include_artists": .array(Array(repeating: .string("Artist"), count: 32))]).values.count == 1)
    for items in [Array(repeating: JSONValue.string("Artist"), count: 33), [.string("")], [.string("A\0B")],
                  [.string("Two\nnames")], [.string(String(repeating: "e\u{301}", count: 65))]] {
        #expect(throws: ContractError.self) { try DJSteeringFilters(["include_artists": .array(items)]) }
    }
    #expect(try DJSteeringFilters(["include_keywords": .array([.string(String(repeating: "e\u{301}", count: 64))])]).values.count == 1)
}

@Test func temporarySteeringSendsOnlyTheFixedSteerContract() throws {
    let change = try makeChange()
    let fields = try change.fields(in: steeringZone)
    #expect(fields == ["zone": .string("Studio@S2"), "constraints": .object(["include_genres": .array([.string("jazz")])]), "for_secs": .integer(3600)])
    #expect(fields["start"] == nil && fields["play"] == nil)
    #expect(change.explanation.contains("replaces the entire temporary session"))
    #expect(change.explanation.contains("Standing taste preferences stay unchanged"))
    #expect(try JSONDecoder().decode([String: JSONValue].self, from: JSONEncoder().encode(fields)) == fields)
}

@Test func steeringDurationIsBoundedAndIndefiniteIsExplicit() throws {
    for seconds: UInt32 in [60, 604800] { #expect(try makeChange(seconds: seconds).seconds == seconds) }
    for seconds: UInt32 in [0, 59, 604801, .max] {
        #expect(throws: ContractError.self) { try makeChange(seconds: seconds) }
    }
    #expect(try makeChange(seconds: nil).fields(in: steeringZone)["for_secs"] == nil)
    #expect(try makeChange(seconds: nil).explanation.contains("until explicitly cleared"))
}

@Test func clearingTemporarySteeringCannotCarryReplacementFields() throws {
    let cleared = try DJSteeringChange.clear(in: steeringZone, before: steerStatus().steering)
    #expect(try cleared.fields(in: steeringZone) == ["zone": .string("Studio@S2"), "clear": .bool(true)])
    #expect(cleared.clearing)
    #expect(throws: ContractError.self) { try makeChange([:]) }
}

@Test func steeringChangeCannotFollowAnotherHouseholdOrNewGroup() throws {
    let change = try makeChange()
    for group in [Zone(household: "S1", coordinatorRoom: "Studio", members: ["Studio"], transportState: "playing"),
                  Zone(household: "S2", coordinatorRoom: "Studio", members: ["Studio", "Hall"], transportState: "playing")] {
        #expect(throws: ContractError.self) { try change.fields(in: group) }
    }
}

@Test func steeringBaselineIgnoresCountdownButBindsOperationalFields() throws {
    let raw = #"{"source":"session","mood":"calm","expires_at":2000000000,"expires_in_secs":3600,"added":{"include_artists":["Example"]},"constraints":{"include_artists":["Example"],"energy_bias":-1}}"#
    let baseline = try steerStatus(raw)
    let change = try DJSteeringChange(in: steeringZone, before: baseline.steering, mood: nil,
                                      filters: DJSteeringFilters(["include_genres": .array([.string("jazz")])]), seconds: nil)
    try change.validate(steerStatus(raw.replacingOccurrences(of: ":3600", with: ":3500")))
    for changed in [raw.replacingOccurrences(of: "session", with: "program"),
                    raw.replacingOccurrences(of: "calm", with: "focus"),
                    raw.replacingOccurrences(of: "2000000000", with: "2000000001"),
                    raw.replacingOccurrences(of: "Example", with: "Different"),
                    raw.replacingOccurrences(of: "-1", with: "-2")] {
        #expect(throws: ContractError.self) { try change.validate(steerStatus(changed)) }
    }
}

@Test func sparseAndExplicitDefaultSteeringBaselinesAreEquivalent() throws {
    let change = try makeChange()
    try change.validate(steerStatus(#"{"source":"none","added":{"energy_bias":0},"constraints":{"allow_long_works":false}}"#))
    #expect(throws: ContractError.self) { try change.validate(steerStatus(#"{"source":"unknown"}"#)) }
}

@Test func selectedMoodIsRecheckedByNameAndDefinition() throws {
    let mood = MoodCatalog.Mood(name: "calm", constraints: ["energy_bias": .integer(-1)])
    let change = try DJSteeringChange(in: steeringZone, before: steerStatus().steering, mood: mood,
                                      filters: DJSteeringFilters(), seconds: 60)
    try change.validateMoods([mood])
    #expect(try change.fields(in: steeringZone)["mood"] == .string("calm"))
    for moods in [[], [mood, mood], [MoodCatalog.Mood(name: "calm", constraints: ["energy_bias": .integer(1)])],
                  [MoodCatalog.Mood(name: "focus", constraints: mood.constraints)]] {
        #expect(throws: ContractError.self) { try change.validateMoods(moods) }
    }
}

@Test func draftImportsOnlySessionAdditionsNotInheritedMoodFilters() throws {
    let status = try steerStatus(#"{"source":"session","mood":"calm","added":{"include_artists":["Example"],"energy_bias":-1},"constraints":{"include_artists":["Example"],"include_genres":["classical"],"energy_bias":-2}}"#)
    let draft = try DJSteeringDraft(in: steeringZone, status: status)
    #expect(draft.entries[.includeArtists] == "Example")
    #expect(draft.entries[.includeGenres] == nil)
    #expect(draft.energyBias == -1)
    #expect(draft.mood == "calm")
    #expect(!draft.untilCleared && draft.minutes == "60")
}

@Test func programFiltersAreNotAccidentallyMadePermanentSessionAdditions() throws {
    let draft = try DJSteeringDraft(in: steeringZone, status: steerStatus(#"{"source":"program","mood":"calm","constraints":{"include_genres":["classical"]}}"#))
    #expect(draft.mood.isEmpty && draft.entries.isEmpty)
    #expect(throws: ContractError.self) { try draft.change(moods: []) }
    #expect(try draft.clear().fields(in: steeringZone)["clear"] == .bool(true))
}

@Test func draftUsesOneNamePerLineAndTypedDecades() throws {
    var draft = try DJSteeringDraft(in: steeringZone, status: steerStatus())
    draft.entries[.includeArtists] = "  Earth, Wind & Fire\n\nAnother artist\n"
    draft.entries[.excludeDecades] = "1980\n1990"
    draft.minimumMinutes = "2"; draft.maximumMinutes = "30"
    let change = try draft.change(moods: [])
    #expect(change.filters.values["include_artists"] == .array([.string("Earth, Wind & Fire"), .string("Another artist")]))
    #expect(change.filters.values["exclude_decades"] == .array([.integer(1980), .integer(1990)]))
    #expect(change.filters.values["max_work_minutes"] == .integer(30))
}

@Test func draftRefusesCoercedOrUnknownInputsBeforeReview() throws {
    var draft = try DJSteeringDraft(in: steeringZone, status: steerStatus())
    draft.entries[.includeGenres] = "jazz"
    for text in ["", "1.5", "1e2", "-1", "+1", "10081", "999999999999999999999999"] {
        draft.minutes = text
        #expect(throws: ContractError.self) { try draft.change(moods: []) }
    }
    draft.minutes = "1"; draft.entries[.decades] = "80s"
    #expect(throws: ContractError.self) { try draft.change(moods: []) }
    draft.entries[.decades] = "1980"; draft.mood = "missing"
    #expect(throws: ContractError.self) { try draft.change(moods: []) }
}

@Test func localEditsCannotMutateAnAlreadyReviewedSteeringValue() throws {
    var draft = try DJSteeringDraft(in: steeringZone, status: steerStatus())
    draft.entries[.includeGenres] = "jazz"
    let reviewed = try draft.change(moods: [])
    draft.entries[.includeGenres] = "rock"; draft.minutes = "120"; draft.untilCleared = true
    #expect(reviewed.filters.values["include_genres"] == .array([.string("jazz")]))
    #expect(reviewed.seconds == 3600)
    #expect(try draft.change(moods: []).seconds == nil)
}

@Test func unknownStoredFiltersCannotBeLostDuringDraftImport() throws {
    for raw in [#"{"source":"session","added":{"future_filter":true}}"#,
                #"{"source":"program","constraints":{"future_filter":true}}"#,
                #"{"source":"session","added":{"include_artists":["Line\nBreak"]}}"#] {
        #expect(throws: ContractError.self) { try DJSteeringDraft(in: steeringZone, status: steerStatus(raw)) }
    }
}
