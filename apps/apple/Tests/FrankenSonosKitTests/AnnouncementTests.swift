import Foundation
import Testing
@testable import FrankenSonosKit

private func announcementZone(_ household: String = "S2", members: [String] = ["Studio", "Hall"]) -> Zone {
    Zone(household: household, coordinatorRoom: "Studio", members: members, transportState: "playing")
}
private func announcement(_ source: Announcement.Source = .speech("Dinner is ready"), rooms: [String] = ["Hall"]) throws -> Announcement {
    try Announcement(source: source, rooms: rooms, in: announcementZone())
}
private func announcementReport(_ json: String) throws -> AnnouncementReport {
    let decoder = JSONDecoder()
    decoder.keyDecodingStrategy = .convertFromSnakeCase
    return try decoder.decode(AnnouncementReport.self, from: Data(json.utf8))
}
private let cleanAnnouncement = #"{"done":"announced in Hall; everything put back","clean":true,"households":[{"lead":"Hall","rooms":["Hall"],"levels":[{"room":"Hall","volume":25}],"outcome":"finished"}]}"#

@Test func announcementSpeechUsesExplicitQualifiedRecipientsAndNoPlaybackRouteFields() throws {
    let request = try announcement(.speech("  Dinner\nA & B 🍽️  "))
    let fields = try request.fields(in: announcementZone())
    #expect(fields == ["text": .string("Dinner\nA & B 🍽️"), "rooms": .array([.string("Hall@S2")]), "volume": .integer(35)])
    let data = try JSONEncoder().encode(fields)
    #expect(try JSONDecoder().decode([String: JSONValue].self, from: data) == fields)
    #expect(request.explanation.contains("Hall@S2"))
    #expect(request.explanation.contains("Dinner\nA & B 🍽️"))
    #expect(Announcement.timeout == 1200)
}

@Test func nativeChimesAreExclusiveSourcesAndZeroVolumeIsPreserved() throws {
    for chime in Announcement.Chime.allCases {
        let request = try Announcement(source: .chime(chime), rooms: ["Studio", "Hall"], volume: 0, in: announcementZone())
        #expect(try request.fields(in: announcementZone()) == [
            "chime": .string(chime.rawValue), "rooms": .array([.string("Studio@S2"), .string("Hall@S2")]), "volume": .integer(0)
        ])
    }
}

@Test func announcementNeverDefaultsEmptyOrInvalidSelectionToTheWholeHouse() throws {
    for rooms in [[], [""], ["Hall", "Hall"], ["Absent"], ["all"], ["Hall@S1"]] as [[String]] {
        #expect(throws: ContractError.self) { try Announcement(source: .chime(.bell), rooms: rooms, in: announcementZone()) }
    }
    for volume in [-1, 101] {
        #expect(throws: ContractError.self) { try Announcement(source: .chime(.beep), rooms: ["Hall"], volume: volume, in: announcementZone()) }
    }
}

@Test func announcementSpeechCountsUnicodeScalarsLikeTheRustBackend() throws {
    #expect(try announcement(.speech(String(repeating: "x", count: 1000))).source == .speech(String(repeating: "x", count: 1000)))
    // 501 graphemes, but 1002 Unicode scalars: the backend counts Rust chars.
    for text in ["", " \n\t ", "a\0b", String(repeating: "x", count: 1001), String(repeating: "e\u{301}", count: 501)] {
        #expect(throws: ContractError.self) { try announcement(.speech(text)) }
    }
}

@Test func announcementIsBoundToReviewedHouseholdAndGroupNotJustDisplayNames() throws {
    let request = try announcement()
    #expect(throws: ContractError.self) { try request.fields(in: announcementZone("S1")) }
    #expect(throws: ContractError.self) { try request.fields(in: announcementZone(members: ["Studio", "Hall", "Office"])) }
    let reordered = announcementZone(members: ["Hall", "Studio"])
    #expect(try request.fields(in: reordered)["rooms"] == .array([.string("Hall@S2")]))
}

@Test func announcementCannotSilentlyNormalizeRoomNamesIntoAnotherIdentity() throws {
    let zone = announcementZone(members: ["Studio", "Büro"])
    #expect(throws: ContractError.self) {
        try Announcement(source: .chime(.rise), rooms: ["Bu\u{308}ro"], in: zone)
    }
}

@Test func cleanAnnouncementAcceptsOmittedOptionalFieldsAndKeepsCappedLevels() throws {
    let report = try announcementReport(cleanAnnouncement).validated(for: announcement())
    #expect(report.clean)
    #expect(report.households[0].notRestored == nil)
    #expect(report.detail.contains("volume 25 after house policy"))
    #expect(report.detail.contains("Clip finished"))
}

@Test func announcementReportsEveryNonCleanEndingWithoutInventingSuccess() throws {
    for outcome in ["deadline", "never_started", "failed"] {
        let error = outcome == "failed" ? #", "error":"Speaker offline""# : ""
        let json = #"{"done":"incomplete","clean":false,"households":[{"lead":"Hall","rooms":["Hall"],"levels":[],"outcome":"ENDING"ERROR}]}"#
            .replacingOccurrences(of: "ENDING", with: outcome).replacingOccurrences(of: "ERROR", with: error)
        let report = try announcementReport(json).validated(for: announcement())
        #expect(!report.clean)
        #expect(report.detail.contains("not repeated"))
        if outcome == "failed" { #expect(report.detail.contains("Speaker offline")) }
        if outcome == "never_started" { #expect(report.detail.contains("never started")) }
        if outcome == "deadline" { #expect(report.detail.contains("deadline")) }
    }
}

@Test func completedClipWithFailedRestorationRemainsPartial() throws {
    let json = cleanAnnouncement.replacingOccurrences(of: #""clean":true"#, with: #""clean":false"#)
        .replacingOccurrences(of: #""outcome":"finished""#, with: #""outcome":"finished","not_restored":["Hall volume: offline","Studio grouping: changed by another controller"]"#)
    let report = try announcementReport(json).validated(for: announcement())
    #expect(!report.clean)
    #expect(report.detail.contains("Not restored: Hall volume: offline"))
    #expect(report.detail.contains("Not restored: Studio grouping: changed by another controller"))
}

@Test func announcementReceiptRejectsMismatchedCoverageAndContradictoryCleanResults() throws {
    let invalid = [
        cleanAnnouncement.replacingOccurrences(of: "Hall", with: "Office"),
        cleanAnnouncement.replacingOccurrences(of: #""rooms":["Hall"]"#, with: #""rooms":["Hall","Hall"]"#),
        cleanAnnouncement.replacingOccurrences(of: #""levels":[{"room":"Hall","volume":25}]"#, with: #""levels":[]"#),
        cleanAnnouncement.replacingOccurrences(of: #""volume":25"#, with: #""volume":101"#),
        cleanAnnouncement.replacingOccurrences(of: #""outcome":"finished""#, with: #""outcome":"deadline""#),
        cleanAnnouncement.replacingOccurrences(of: #""outcome":"finished""#, with: #""outcome":"finished","not_restored":["not restored"]"#),
        cleanAnnouncement.replacingOccurrences(of: #""outcome":"finished""#, with: #""outcome":"failed""#),
        cleanAnnouncement.replacingOccurrences(of: #""outcome":"finished""#, with: #""outcome":"finished","error":"failed""#),
        #"{"done":"ok","clean":true,"households":[]}"#,
    ]
    for json in invalid {
        #expect(throws: (any Error).self) { try announcementReport(json).validated(for: announcement()) }
    }
}

@Test func announcementReportRequiresOneHouseholdAndDistinctKnownLevels() throws {
    let first = #"{"lead":"Hall","rooms":["Hall"],"levels":[{"room":"Hall","volume":20},{"room":"Hall","volume":20}],"outcome":"finished"}"#
    for json in [#"{"done":"ok","clean":false,"households":[FIRST]}"#.replacingOccurrences(of: "FIRST", with: first),
                 #"{"done":"ok","clean":false,"households":[FIRST,FIRST]}"#.replacingOccurrences(of: "FIRST", with: first)] {
        #expect(throws: (any Error).self) { try announcementReport(json).validated(for: announcement()) }
    }
}

@Test func unknownOrMalformedAnnouncementOutcomesCannotAcknowledgeAnAction() throws {
    for json in ["{}", #"{"done":"ok","changed":true}"#,
                 cleanAnnouncement.replacingOccurrences(of: "finished", with: "future_value"),
                 cleanAnnouncement.replacingOccurrences(of: #""not_restored":[]"#, with: #""not_restored":false"#).replacingOccurrences(of: #""clean":true"#, with: #""clean":"true"#)] {
        #expect(throws: (any Error).self) { try announcementReport(json).validated(for: announcement()) }
    }
}
