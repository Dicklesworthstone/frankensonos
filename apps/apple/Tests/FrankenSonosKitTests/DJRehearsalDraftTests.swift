import Foundation
import Testing
@testable import FrankenSonosKit

private func draftZone() -> Zone {
    Zone(household: "S2", coordinatorRoom: "Studio", members: ["Studio"], transportState: "stopped")
}
private func fields(_ draft: DJRehearsalDraft) throws -> [String: JSONValue] {
    try JSONDecoder().decode([String: JSONValue].self, from: JSONEncoder().encode(draft.request(for: draftZone())))
}

@Test func previewDraftKeepsWorkAndHourLengthsMutuallyExclusive() throws {
    var draft = DJRehearsalDraft()
    #expect(try fields(draft)["works"] == .integer(12))
    #expect(try fields(draft)["hours"] == nil)
    draft.works = 500
    #expect(try draft.request(for: draftZone()).works == 500)
    draft.length = .hours; draft.hours = 48
    #expect(try fields(draft)["works"] == nil)
    #expect(try fields(draft)["hours"] == .integer(48))
    for invalid in [0, -1, 49] {
        draft.hours = invalid
        #expect(throws: ContractError.self) { try draft.request(for: draftZone()) }
    }
    draft.length = .works
    for invalid in [0, -1, 501] {
        draft.works = invalid
        #expect(throws: ContractError.self) { try draft.request(for: draftZone()) }
    }
}

@Test func previewDraftPreservesEverySupportedFilterAndLiteralComma() throws {
    var draft = DJRehearsalDraft()
    for filter in DJRehearsalDraft.Filter.allCases {
        switch filter {
        case .decades, .excludeDecades: draft.filters[filter] = "1960\n1980"
        case .periods, .excludePeriods: draft.filters[filter] = "Baroque\nLate Romantic"
        default: draft.filters[filter] = " A, B \n\n C \r\n"
        }
    }
    let constraints = try draft.request(for: draftZone()).constraints
    #expect(constraints.count == 12)
    #expect(constraints["include_artists"] == .array([.string("A, B"), .string("C")]))
    #expect(constraints["decades"] == .array([.integer(1960), .integer(1980)]))
    #expect(constraints["periods"] == .array([.string("baroque"), .string("late_romantic")]))
    #expect(constraints["exclude_keywords"] == .array([.string("A, B"), .string("C")]))
}

@Test func previewDraftRejectsInvalidTimeWithoutInterpretingPhoneTimezone() throws {
    var draft = DJRehearsalDraft(); draft.start = .houseTime
    for time in ["00:00", "23:59", " 09:05 "] {
        draft.houseTime = time
        #expect(try draft.request(for: draftZone()).at == time.trimmingCharacters(in: .whitespacesAndNewlines))
    }
    for time in ["24:00", "23:60", "9:00", "09:0", "09:00Z", "tomorrow", ""] {
        draft.houseTime = time
        #expect(throws: ContractError.self) { try draft.request(for: draftZone()) }
    }
    draft.start = .now
    #expect(try fields(draft)["at"] == nil)
}

@Test func previewDraftSeedsNeverRoundThroughFloatingPoint() throws {
    var draft = DJRehearsalDraft()
    for seed in ["0", "9007199254740993", String(UInt64.max)] {
        draft.seed = seed
        let request = try draft.request(for: draftZone())
        #expect(request.seed == UInt64(seed))
        let json = String(decoding: try JSONEncoder().encode(request), as: UTF8.self)
        #expect(json.contains(seed))
    }
    for seed in ["", "-1", "+1", "1.5", "1e3", "18446744073709551616", "١"] {
        draft.seed = seed
        #expect(throws: ContractError.self) { try draft.request(for: draftZone()) }
    }
}

@Test func previewDraftEmptyOverridesDoNotReplaceInheritedSteering() throws {
    var draft = DJRehearsalDraft()
    draft.mood = " \n "; draft.filters[.includeGenres] = " \n "
    #expect(try !draft.request(for: draftZone()).overridesSteering)
    #expect(try fields(draft)["constraints"] == nil)
    draft.energyBias = -2; draft.allowLongWorks = true
    let changed = try draft.request(for: draftZone())
    #expect(changed.overridesSteering)
    #expect(changed.constraints["energy_bias"] == .integer(-2))
    #expect(changed.constraints["allow_long_works"] == .bool(true))
    draft.energyBias = 3
    #expect(throws: ContractError.self) { try draft.request(for: draftZone()) }
}

@Test func previewDraftBoundsFilterPasteAndWorkLengthInputs() throws {
    var draft = DJRehearsalDraft()
    for entries in [String(repeating: "x", count: 257), Array(repeating: "x", count: 65).joined(separator: "\n"),
                    String(repeating: "x", count: 65_537)] {
        draft.filters[.includeGenres] = entries
        #expect(throws: ContractError.self) { try draft.request(for: draftZone()) }
    }
    draft.filters = [:]; draft.minimumMinutes = "20"; draft.maximumMinutes = "10"
    #expect(throws: ContractError.self) { try draft.request(for: draftZone()) }
    draft.maximumMinutes = "30"
    #expect(try draft.request(for: draftZone()).constraints["min_work_minutes"] == .integer(20))
    for value in ["-1", "2.5", "4294967296"] {
        draft.minimumMinutes = value
        #expect(throws: ContractError.self) { try draft.request(for: draftZone()) }
    }
}

@Test func previewRequestDoesNotFollowLaterComposerEdits() throws {
    var draft = DJRehearsalDraft(); draft.mood = "calm"; draft.filters[.includeGenres] = "jazz"
    let request = try draft.request(for: draftZone())
    draft.mood = "bright"; draft.filters[.includeGenres] = "pop"; draft.works = 100
    #expect(request.mood == "calm" && request.works == 12)
    #expect(request.constraints["include_genres"] == .array([.string("jazz")]))
}

@Test func previewEmptySetAcceptsOmittedOptionalFieldsWithoutInventingPicks() throws {
    let json = #"{"zone":"Studio","from":"2026-10-21T03:00:00Z","seed":1,"works":[],"summary":{"works":0,"minutes":0,"distinct_artists":0,"explicit":0,"hours":[]}}"#
    let request = try DJRehearsalDraft().request(for: draftZone())
    let report = try DaemonClient.decode(DJRehearsal.self, from: Data(json.utf8)).validated(for: request)
    #expect(report.works.isEmpty && report.relaxedWorkCount == 0)
    #expect(DJRehearsal.offset(report.from) == "UTC")
    #expect(report.text(for: request).contains("0 works"))
}

@Test func previewSummaryCannotHideMissingWorksOrFabricateEmptySetTotals() throws {
    let template = #"{"zone":"Studio","from":"2026-10-21T03:00:00Z","seed":1,"works":[],"summary":{"works":0,"minutes":0,"distinct_artists":0,"explicit":0,"hours":[]}}"#
    let request = try DJRehearsalDraft().request(for: draftZone())
    for field in ["works", "minutes", "distinct_artists", "explicit"] {
        let json = template.replacingOccurrences(of: "\"\(field)\":0", with: "\"\(field)\":1")
        #expect(throws: ContractError.self) {
            try DaemonClient.decode(DJRehearsal.self, from: Data(json.utf8)).validated(for: request)
        }
    }
}

@Test func previewHouseClockResponseMustMatchTheRequestedStart() throws {
    var draft = DJRehearsalDraft(); draft.start = .houseTime; draft.houseTime = "23:00"
    let request = try draft.request(for: draftZone())
    let json = #"{"zone":"Studio","from":"2026-10-21T03:00:00Z","seed":1,"works":[],"summary":{"works":0,"minutes":0,"distinct_artists":0,"explicit":0,"hours":[]}}"#
    #expect(throws: ContractError.self) {
        try DaemonClient.decode(DJRehearsal.self, from: Data(json.utf8)).validated(for: request)
    }
}
