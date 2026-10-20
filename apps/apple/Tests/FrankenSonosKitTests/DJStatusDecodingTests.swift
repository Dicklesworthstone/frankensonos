import Foundation
import Testing
@testable import FrankenSonosKit

private let idleDJ = #"{"zone":"Studio","running":false,"steering":{"source":"none"}}"#
private let workJSON = #"{"title":"Example work","composer":"Example artist","movements":3,"movement":2,"movement_title":"Second movement","reason":{"summary":"Variety","factors":[{"factor":"energy_fit","weight":1200}],"relaxed":["length"]}}"#
private func decodeDJ(_ json: String) throws -> DJStatus {
    let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
    return try decoder.decode(DJStatus.self, from: Data(json.utf8))
}
private let statusZone = Zone(household: "S2", coordinatorRoom: "Studio", members: ["Studio"], transportState: "stopped")

@Test func stoppedDJWithSerdeOmittedEmptyQueueLoads() throws {
    let status = try decodeDJ(idleDJ).validated(for: statusZone)
    #expect(!status.running)
    #expect(status.now == nil)
    #expect(status.next.isEmpty)
    #expect(status.steering.source == "none")
    #expect(status.problem == nil)
}

@Test func runningDJWithoutQueuedSuccessorsIsStillReadable() throws {
    let json = idleDJ.replacingOccurrences(of: #""running":false"#, with: #""running":true,"now":WORK"#)
        .replacingOccurrences(of: "WORK", with: workJSON)
    let status = try decodeDJ(json).validated(for: statusZone)
    #expect(status.running)
    #expect(status.now?.movement == 2)
    #expect(status.next.isEmpty)
}

@Test func explicitAndOmittedEmptyDJQueuesAreEquivalent() throws {
    let explicit = idleDJ.replacingOccurrences(of: #""running":false"#, with: #""running":false,"next":[]"#)
    #expect(try decodeDJ(idleDJ) == decodeDJ(explicit))
}

@Test func populatedDJQueueAndSelectionExplanationsAreNotDiscarded() throws {
    let json = idleDJ.replacingOccurrences(of: #""running":false"#, with: #""running":true,"next":[WORK]"#)
        .replacingOccurrences(of: "WORK", with: workJSON)
    let status = try decodeDJ(json).validated(for: statusZone)
    #expect(status.next.count == 1)
    #expect(status.next[0].title == "Example work")
    #expect(status.next[0].movementTitle == "Second movement")
    #expect(status.next[0].reason.factors?.first?.weight == 1200)
    #expect(status.next[0].reason.relaxed == ["length"])
}

@Test func malformedPresentDJQueuesDoNotBecomeEmpty() {
    for value in ["null", "{}", "false", #""[]""#, "[null]", "[{}]"] {
        let json = idleDJ.replacingOccurrences(of: #""running":false"#, with: #""running":false,"next":VALUE"#)
            .replacingOccurrences(of: "VALUE", with: value)
        #expect(throws: (any Error).self) { try decodeDJ(json) }
    }
}

@Test func sparseDJDecodingStillRequiresIdentityRunningAndSteering() {
    for json in ["{}", #"{"zone":"Studio","running":false}"#,
                 #"{"zone":"Studio","steering":{"source":"none"}}"#,
                 #"{"running":false,"steering":{"source":"none"}}"#,
                 #"{"zone":"Studio","running":false,"steering":null}"#,
                 #"{"zone":"Studio","running":"false","steering":{"source":"none"}}"#] {
        #expect(throws: (any Error).self) { try decodeDJ(json) }
    }
    #expect(throws: ContractError.self) {
        try decodeDJ(idleDJ.replacingOccurrences(of: "Studio", with: "Other")).validated(for: statusZone)
    }
}

@Test func sparseDJStatusRetainsSessionExpiryFiltersAndProblem() throws {
    let json = #"{"zone":"Studio","running":false,"steering":{"source":"session","mood":"calm","expires_at":2000000000,"expires_in_secs":60,"added":{"include_genres":["jazz"]},"constraints":{"include_genres":["jazz"],"energy_bias":-1}},"problem":"Library refresh needed"}"#
    let status = try decodeDJ(json).validated(for: statusZone)
    #expect(status.steering.expiresAt == 2_000_000_000)
    #expect(status.steering.expiresInSecs == 60)
    #expect(status.steering.added?["include_genres"] == .array([.string("jazz")]))
    #expect(status.steering.constraints?["energy_bias"] == .integer(-1))
    #expect(status.problem == "Library refresh needed")
}

@Test func sparseDJStatusPreservesEncodingAndMemberwiseConstruction() throws {
    let status = try decodeDJ(idleDJ)
    let reconstructed = DJStatus(zone: status.zone, running: status.running, now: status.now,
                                 next: status.next, steering: status.steering, problem: status.problem)
    #expect(status == reconstructed)
    let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
    let encoded = try encoder.encode(status)
    #expect(try decodeDJ(String(decoding: encoded, as: UTF8.self)) == status)
}
