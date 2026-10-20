import Foundation
import Testing
@testable import FrankenSonosKit

private func composerZone(household: String = "S2", coordinator: String = "Studio",
                          members: [String] = ["Studio", "Hall"], state: String = "playing") -> Zone {
    Zone(household: household, coordinatorRoom: coordinator, members: members, transportState: state)
}

@Test func announcementComposerRequiresDeliberateRecipientSelection() throws {
    let zone = composerZone()
    var form = AnnouncementForm(group: zone)
    form.speech = "Ready"
    #expect(throws: ContractError.self) { try form.announcement(in: zone) }
    let selected = form.select("Hall", selected: true)
    #expect(selected)
    #expect(try form.announcement(in: zone).rooms == ["Hall"])
    form.clearRooms()
    #expect(throws: ContractError.self) { try form.announcement(in: zone) }
}

@Test func announcementComposerNeverExpandsAnUnknownRoomIntoAllRooms() {
    var form = AnnouncementForm(group: composerZone())
    let all = form.select("all", selected: true)
    #expect(!all)
    let other = form.select("Hall@S1", selected: true)
    #expect(!other)
    #expect(form.selectedRooms.isEmpty)
}

@Test func announcementComposerSourceSwitchDoesNotMixSpeechAndChimes() throws {
    let zone = composerZone()
    var form = AnnouncementForm(group: zone)
    form.speech = "Kept draft"
    form.selectAll()
    form.volume = 0
    form.kind = .chime; form.chime = .rise
    #expect(try form.announcement(in: zone).source == .chime(.rise))
    #expect(try form.announcement(in: zone).volume == 0)
    form.kind = .speech
    #expect(try form.announcement(in: zone).source == .speech("Kept draft"))
    #expect(try form.announcement(in: zone).rooms == ["Studio", "Hall"])
}

@Test func announcementComposerClearsRecipientsOnAnyGroupIdentityChange() throws {
    let initial = composerZone()
    for current in [composerZone(household: "S1"), composerZone(coordinator: "Hall"),
                    composerZone(members: ["Studio", "Hall", "Office"])] {
        var form = AnnouncementForm(group: initial)
        form.speech = "Keep the text"; form.volume = 22; form.selectAll()
        #expect(throws: ContractError.self) { try form.announcement(in: current) }
        form.observe(current)
        #expect(form.selectedRooms.isEmpty)
        #expect(form.speech == "Keep the text")
        #expect(form.volume == 22)
        #expect(form.notice != nil)
        #expect(throws: ContractError.self) { try form.announcement(in: current) }
        form.selectAll()
        #expect(try form.announcement(in: current).group.sameGroup(as: current))
    }
}

@Test func announcementComposerRetainsRecipientsAcrossHarmlessPlaybackChanges() throws {
    var form = AnnouncementForm(group: composerZone())
    form.speech = "Ready"; form.select("Hall", selected: true)
    let current = composerZone(members: ["Hall", "Studio"], state: "paused")
    form.observe(current)
    #expect(form.selectedRooms == ["Hall"])
    #expect(form.notice == nil)
    #expect(try form.announcement(in: current).rooms == ["Hall"])
}

@Test func announcementReviewValueDoesNotFollowLaterComposerEdits() throws {
    let zone = composerZone()
    var form = AnnouncementForm(group: zone)
    form.speech = "First message"; form.select("Hall", selected: true)
    let reviewed = try form.announcement(in: zone)
    form.speech = "Different message"; form.volume = 80; form.selectAll()
    #expect(reviewed.source == .speech("First message"))
    #expect(reviewed.rooms == ["Hall"])
    #expect(reviewed.volume == 35)
}
