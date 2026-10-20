import Foundation
import Testing
@testable import FrankenSonosKit

private func settingsDecode<T: Decodable>(_ type: T.Type, _ json: String) throws -> T {
    let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
    return try decoder.decode(type, from: Data(json.utf8))
}
private func settingsZone(_ home: String = "HOUSE_A", room: String = "Studio", members: [String] = ["Studio", "Kitchen"], state: String = "playing") -> Zone {
    Zone(household: home, coordinatorRoom: room, members: members, transportState: state)
}
private func settingsTone(_ room: String = "Kitchen", bass: Int = 0, treble: Int = 0, loudness: Bool = true) -> SpeakerTone {
    SpeakerTone(room: room, bass: bass, treble: treble, loudness: loudness)
}
private func settingsOrder(_ mode: QueuePlaybackMode = .normal, queue: Bool = true, dj: Bool = false) -> QueuePlaybackOrder {
    QueuePlaybackOrder(zone: "Studio", mode: mode, queueActive: queue, djActive: dj)
}

@Suite("Native speaker sound and queue order contracts") struct ListeningSettingsTests {
    @Test func allSixModesRoundTripWithoutConfusingShuffleAndRepeat() throws {
        for mode in QueuePlaybackMode.allCases {
            #expect(QueuePlaybackMode(shuffled: mode.shuffled, repetition: mode.repetition) == mode)
            #expect(try settingsDecode(QueuePlaybackMode.self, "\"\(mode.rawValue)\"") == mode)
        }
        #expect(QueuePlaybackMode(shuffled: true, repetition: .off) == .shuffleNoRepeat)
        #expect(QueuePlaybackMode(shuffled: true, repetition: .queue) == .shuffle)
        #expect(QueuePlaybackMode(shuffled: false, repetition: .track) == .repeatOne)
    }
    @Test func unknownModesAndMissingFactsAreNotAssumedNormal() {
        for json in [#"{"zone":"Studio","mode":"FUTURE_MODE","queue_active":true,"dj_active":false}"#,
                     #"{"zone":"Studio","mode":"NORMAL","queue_active":true}"#,
                     #"{"zone":"Studio","mode":"NORMAL","queue_active":1,"dj_active":false}"#] {
            #expect(throws: (any Error).self) { try settingsDecode(QueuePlaybackOrder.self, json) }
        }
    }
    @Test func toneRejectsBadRangesTypesAndWrongRoom() throws {
        for bass in [-11, 11, Int.min, Int.max] {
            #expect(throws: ContractError.self) { try settingsTone(bass: bass).validated(room: "Kitchen") }
        }
        #expect(throws: ContractError.self) { try settingsTone().validated(room: "Studio") }
        for json in [#"{"room":"Kitchen","bass":0,"treble":0}"#,
                     #"{"room":"Kitchen","bass":0.5,"treble":0,"loudness":false}"#,
                     #"{"room":"Kitchen","bass":"0","treble":0,"loudness":false}"#,
                     #"{"room":"Kitchen","bass":0,"treble":0,"loudness":0}"#] {
            #expect(throws: (any Error).self) { try settingsDecode(SpeakerTone.self, json) }
        }
        #expect(try settingsTone(bass: -10, treble: 10).validated(room: "Kitchen").bass == -10)
    }
    @Test func toneSendsOnlyEditedControlsToTheNamedMember() throws {
        let change = try SpeakerToneChange(target: RoomID(household: "HOUSE_A", room: "Kitchen"),
            before: settingsTone(), bass: 3, treble: 0, loudness: true)
        #expect(try change.fields(in: settingsZone()) == ["zone": .string("Kitchen@HOUSE_A"), "bass": .integer(3)])
        let flat = try SpeakerToneChange(target: change.target, before: settingsTone(bass: 3, treble: 4),
            bass: 0, treble: 0, loudness: false)
        #expect(try flat.fields(in: settingsZone()) == ["zone": .string("Kitchen@HOUSE_A"), "bass": .integer(0), "treble": .integer(0), "loudness": .bool(false)])
    }
    @Test func noOpAndCrossHouseholdToneChangesAreRefused() throws {
        let target = RoomID(household: "HOUSE_A", room: "Kitchen")
        #expect(throws: ContractError.self) {
            try SpeakerToneChange(target: target, before: settingsTone(), bass: 0, treble: 0, loudness: true)
        }
        let change = try SpeakerToneChange(target: target, before: settingsTone(), bass: 1, treble: 0, loudness: true)
        #expect(throws: ContractError.self) { try change.fields(in: settingsZone("HOUSE_B")) }
        #expect(throws: ContractError.self) { try change.fields(in: settingsZone(members: ["Studio"])) }
    }
    @Test func tonePreflightProtectsEvenUneditedFields() throws {
        let change = try SpeakerToneChange(target: RoomID(household: "HOUSE_A", room: "Kitchen"),
            before: settingsTone(), bass: 3, treble: 0, loudness: true)
        try change.validate(settingsTone())
        #expect(throws: ContractError.self) { try change.validate(settingsTone(treble: 1)) }
        #expect(throws: ContractError.self) { try change.validate(settingsTone(loudness: false)) }
    }
    @Test func orderWritesOneCompleteModeAndItsObservedPrecondition() throws {
        let change = try PlaybackOrderChange(group: settingsZone(), before: settingsOrder(), desired: .shuffleNoRepeat)
        #expect(try change.fields(in: settingsZone(state: "paused")) == [
            "zone": .string("Studio@HOUSE_A"), "mode": .string("SHUFFLE_NOREPEAT"), "expected_mode": .string("NORMAL")])
        #expect(throws: ContractError.self) { try change.fields(in: settingsZone("HOUSE_B")) }
        #expect(throws: ContractError.self) { try change.fields(in: settingsZone(members: ["Studio"])) }
    }
    @Test func djProtectionPermitsOnlyReturningToSequentialOrder() throws {
        for mode in QueuePlaybackMode.allCases where mode != .normal {
            #expect(throws: ContractError.self) {
                try PlaybackOrderChange(group: settingsZone(), before: settingsOrder(dj: true), desired: mode)
            }
        }
        let recovery = try PlaybackOrderChange(group: settingsZone(), before: settingsOrder(.shuffle, dj: true), desired: .normal)
        #expect(recovery.desired == .normal)
    }
    @Test func radioCannotBeReplacedAndNoOpIsNotACommand() {
        #expect(throws: ContractError.self) {
            try PlaybackOrderChange(group: settingsZone(), before: settingsOrder(queue: false), desired: .shuffle)
        }
        #expect(throws: ContractError.self) {
            try PlaybackOrderChange(group: settingsZone(), before: settingsOrder(), desired: .normal)
        }
    }
    @Test func sourceDJOrModeChangesInvalidateTheReviewedOrder() throws {
        let change = try PlaybackOrderChange(group: settingsZone(), before: settingsOrder(), desired: .repeatAll)
        try change.validate(settingsOrder())
        for now in [settingsOrder(.repeatOne), settingsOrder(queue: false), settingsOrder(dj: true)] {
            #expect(throws: ContractError.self) { try change.validate(now) }
        }
    }
    @Test func toneReceiptUsesActualEqDTOAndRejectsPartialChanges() throws {
        let change = try SpeakerToneChange(target: RoomID(household: "HOUSE_A", room: "Kitchen"),
            before: settingsTone(), bass: 2, treble: -1, loudness: false)
        let good = try settingsDecode(SpeakerToneReply.self, #"{"done":"Sound applied","eq":{"room":"Kitchen","bass":2,"treble":-1,"loudness":false}}"#)
        #expect(try good.validated(for: change).eq == change.desired)
        let partial = SpeakerToneReply(done: "Partial", eq: settingsTone(bass: 2))
        #expect(throws: ContractError.self) { try partial.validated(for: change) }
        #expect(throws: ContractError.self) { try SpeakerToneReply(done: "", eq: change.desired).validated(for: change) }
    }
    @Test func recoveredOrderReceiptIsAnObservationNotInventedCausality() throws {
        let change = try PlaybackOrderChange(group: settingsZone(), before: settingsOrder(), desired: .shuffleNoRepeat)
        let reply = try settingsDecode(PlaybackOrderReply.self, #"{"done":"Mode observed","state":{"zone":"Studio","mode":"SHUFFLE_NOREPEAT","queue_active":true,"dj_active":false},"changed":true,"recovered":true}"#)
        #expect(try reply.validated(for: change).recovered)
        #expect(reply.detail.contains("does not prove"))
        for bad in [settingsOrder(), settingsOrder(.shuffleNoRepeat, queue: false), settingsOrder(.shuffleNoRepeat, dj: true)] {
            #expect(throws: ContractError.self) { try PlaybackOrderReply(done: "OK", state: bad, changed: true, recovered: false).validated(for: change) }
        }
    }
    @Test func missingReceiptFieldsCannotAcknowledgeAWrite() {
        for json in [#"{"done":"OK","state":{"zone":"Studio","mode":"NORMAL","queue_active":true,"dj_active":false},"changed":true}"#,
                     #"{"done":"OK","changed":true,"recovered":false}"#] {
            #expect(throws: (any Error).self) { try settingsDecode(PlaybackOrderReply.self, json) }
        }
        #expect(throws: (any Error).self) { try settingsDecode(SpeakerToneReply.self, #"{"done":"OK","changed":true}"#) }
    }
    @Test func scopeIgnoresClockStateButNeverMembershipOrHousehold() {
        #expect(ListeningSettingsScope.order(settingsZone()) == .order(settingsZone(state: "paused")))
        #expect(ListeningSettingsScope.order(settingsZone()) != .order(settingsZone("HOUSE_B")))
        #expect(ListeningSettingsScope.speaker(settingsZone(), room: "Kitchen") != .speaker(settingsZone(), room: "Studio"))
        #expect(throws: ContractError.self) { try ListeningSettingsScope.speaker(settingsZone(), room: "Gone").validated() }
    }
    @Test func valuesAreBoundToTheRequestedControlAndSpeaker() {
        #expect(throws: ContractError.self) { try ListeningSettingsValue.order(settingsOrder()).validated(for: .speaker(settingsZone(), room: "Kitchen")) }
        #expect(throws: ContractError.self) { try ListeningSettingsValue.speaker(settingsTone()).validated(for: .speaker(settingsZone(), room: "Studio")) }
    }
}

private actor SettingsReader {
    var calls: [ListeningSettingsScope] = []
    var value: ListeningSettingsValue = .speaker(settingsTone())
    var fail = false
    var hold = false
    var waiting: CheckedContinuation<ListeningSettingsValue, any Error>?
    func set(_ value: ListeningSettingsValue) { self.value = value }
    func setFailure(_ enabled: Bool) { fail = enabled }
    func setHold() { hold = true }
    func release(_ value: ListeningSettingsValue) { waiting?.resume(returning: value); waiting = nil }
    func read(_ scope: ListeningSettingsScope) async throws -> ListeningSettingsValue {
        calls.append(scope)
        if hold {
            hold = false
            return try await withCheckedThrowingContinuation { waiting = $0 }
        }
        if fail { throw ContractError.invalid("fixture offline") }
        return value
    }
}
@MainActor private func settingsEventually(_ condition: () async -> Bool) async throws {
    for _ in 0..<2_000 {
        if await condition() { return }
        try await Task.sleep(for: .milliseconds(1))
    }
    Issue.record("Listening-settings operation did not settle")
    throw ContractError.invalid("Test did not settle")
}

@Suite("Native listening settings lifecycle") @MainActor struct ListeningSettingsModelTests {
    private let scope = ListeningSettingsScope.speaker(settingsZone(), room: "Kitchen")
    private func model(_ reader: SettingsReader) -> ListeningSettingsModel {
        ListeningSettingsModel(now: { Date(timeIntervalSince1970: 100) }, read: { try await reader.read($0) })
    }
    @Test func startsLazilyAndNeverPollsOrWrites() async throws {
        let read = SettingsReader(), model = model(SettingsReader())
        #expect(!model.active)
        #expect(model.selection() == nil)
        let subject = self.model(read)
        subject.activate(scope); try await settingsEventually { subject.isFresh }
        subject.activate(scope)
        try await Task.sleep(for: .milliseconds(10))
        #expect(await read.calls.count == 1)
        subject.stop(); model.stop()
    }
    @Test func editsRemainLocalAndPreserveExplicitZeroAndFalse() async throws {
        let read = SettingsReader(), model = model(read)
        model.activate(scope); try await settingsEventually { model.isFresh }
        model.editTone(bass: 3); model.editTone(bass: 0, loudness: false)
        let selection = try #require(model.selection())
        guard case .speaker(let change) = try model.resolve(selection) else { Issue.record("Wrong control"); return }
        #expect(try change.fields(in: settingsZone()) == ["zone": .string("Kitchen@HOUSE_A"), "loudness": .bool(false)])
        #expect(await read.calls.count == 1)
        model.stop()
    }
    @Test func invalidEditsAndNoOpsNeverCreateAReview() async throws {
        let read = SettingsReader(), model = model(read)
        model.activate(scope); try await settingsEventually { model.isFresh }
        model.editTone(bass: 99); model.editOrder(shuffled: true)
        #expect(model.selection() == nil)
        model.editTone(treble: 2); model.discard(); #expect(!model.hasEdits)
        model.stop()
    }
    @Test func editingOrRefreshingConsumesTheOldSelection() async throws {
        let read = SettingsReader(), model = model(read)
        model.activate(scope); try await settingsEventually { model.isFresh }
        model.editTone(bass: 2); let selection = try #require(model.selection())
        model.editTone(bass: 3)
        #expect(throws: ContractError.self) { try model.resolve(selection) }
        let another = try #require(model.selection()); model.refresh()
        #expect(throws: ContractError.self) { try model.resolve(another) }
        try await settingsEventually { model.isFresh }
        #expect(model.tone?.bass == 3, "unchanged observations retain local edits")
        #expect(throws: ContractError.self) { try model.resolve(another) }
        model.stop()
    }
    @Test func changedObservationDiscardsDraftWithoutSendingIt() async throws {
        let read = SettingsReader(), model = model(read)
        model.activate(scope); try await settingsEventually { model.isFresh }
        model.editTone(bass: 2)
        await read.set(.speaker(settingsTone(treble: 4)))
        model.refresh(); try await settingsEventually { model.isFresh }
        #expect(!model.hasEdits); #expect(model.tone?.treble == 4)
        #expect(model.notice?.contains("nothing was sent") == true)
        model.stop()
    }
    @Test func failureRetainsLastObservationTimeButNoActionAuthority() async throws {
        let read = SettingsReader(), model = model(read)
        model.activate(scope); try await settingsEventually { model.isFresh }
        let date = model.observedAt; model.editTone(bass: 2)
        await read.setFailure(true); model.refresh()
        try await settingsEventually { !model.loading }
        #expect(model.observedAt == date); #expect(!model.canEdit); #expect(model.hasEdits)
        #expect(model.selection() == nil); #expect(model.problem != nil)
        await read.setFailure(false); model.refresh(); try await settingsEventually { model.isFresh }
        #expect(model.tone?.bass == 2); model.stop()
    }
    @Test func switchedRoomWaitsForOldReadAndOnlyUsesNewestIntent() async throws {
        let read = SettingsReader(), model = model(read)
        await read.setHold(); model.activate(scope)
        try await settingsEventually { await read.waiting != nil }
        model.activate(.speaker(settingsZone("HOUSE_B"), room: "Kitchen"))
        model.activate(.speaker(settingsZone(), room: "Studio"))
        #expect(await read.calls.count == 1)
        await read.set(.speaker(settingsTone("Studio", bass: 4)))
        await read.release(.speaker(settingsTone(bass: -4)))
        try await settingsEventually { model.isFresh }
        #expect(await read.calls.count == 2); #expect(model.tone?.room == "Studio"); #expect(model.tone?.bass == 4)
        model.stop()
    }
    @Test func sameNamedHouseholdChangeCannotUseOldDraft() async throws {
        let read = SettingsReader(), model = model(read)
        model.activate(scope); try await settingsEventually { model.isFresh }
        model.editTone(bass: 2); let selection = try #require(model.selection())
        model.activate(.speaker(settingsZone("HOUSE_B"), room: "Kitchen"))
        #expect(throws: ContractError.self) { try model.resolve(selection) }
        try await settingsEventually { model.isFresh }
        #expect(!model.hasEdits); #expect(model.scope?.target.household == "HOUSE_B")
        model.stop()
    }
    @Test func suspensionCancelsUnsentReadAndDoesNotReviveSelections() async throws {
        let read = SettingsReader(), model = model(read)
        await read.setHold(); model.activate(scope)
        try await settingsEventually { await read.waiting != nil }
        model.suspend(); await read.release(.speaker(settingsTone(bass: 4)))
        try await Task.sleep(for: .milliseconds(10))
        #expect(!model.isFresh); #expect(model.observed == nil); #expect(await read.calls.count == 1)
        model.activate(scope); try await settingsEventually { model.isFresh }
        #expect(model.tone?.bass == 0); model.stop()
    }
    @Test func terminalStopDoesNotRestartOrAcceptAStaleResult() async throws {
        let read = SettingsReader(), model = model(read)
        await read.setHold(); model.activate(scope)
        try await settingsEventually { await read.waiting != nil }
        model.stop(); model.activate(scope); model.refresh()
        await read.release(.speaker(settingsTone()))
        try await Task.sleep(for: .milliseconds(10))
        #expect(!model.active); #expect(model.observed == nil); #expect(await read.calls.count == 1)
    }
    @Test func invalidScopeDisablesOldControlsWithoutRetargeting() async throws {
        let read = SettingsReader(), model = model(read)
        model.activate(scope); try await settingsEventually { model.isFresh }
        model.editTone(bass: 2)
        model.activate(.speaker(settingsZone(), room: "Gone"))
        #expect(!model.active); #expect(model.selection() == nil); #expect(model.problem != nil)
        #expect(await read.calls.count == 1); model.stop()
    }
    @Test func modeDraftCombinesControlsWithoutSeparateWrites() async throws {
        let read = SettingsReader(), model = model(read)
        await read.set(.order(settingsOrder()))
        model.activate(.order(settingsZone())); try await settingsEventually { model.isFresh }
        model.editOrder(shuffled: true); #expect(model.order?.mode == .shuffleNoRepeat)
        model.editOrder(repetition: .queue); #expect(model.order?.mode == .shuffle)
        model.editOrder(shuffled: false); #expect(model.order?.mode == .repeatAll)
        guard case .order(let change) = try model.resolve(#require(model.selection())) else { Issue.record("Wrong selection"); return }
        #expect(change.before.mode == .normal); #expect(change.desired == .repeatAll)
        #expect(await read.calls.count == 1); model.stop()
    }
    @Test func activeDJCannotBeShuffledButCanRecoverWholeWorkOrder() async throws {
        let read = SettingsReader(), model = model(read)
        await read.set(.order(settingsOrder(.shuffle, dj: true)))
        model.activate(.order(settingsZone())); try await settingsEventually { model.isFresh }
        model.editOrder(repetition: .track); #expect(model.order?.mode == .shuffle)
        model.useSequentialOrder(); #expect(model.order?.mode == .normal)
        #expect(model.selection() != nil); model.stop()
    }
    @Test func malformedInjectedValuesAreCheckedInsideTheProductionModel() async throws {
        let read = SettingsReader(), model = model(read)
        await read.set(.speaker(settingsTone(bass: 20)))
        model.activate(scope); try await settingsEventually { !model.loading }
        #expect(!model.isFresh); #expect(model.observedAt == nil); #expect(model.problem != nil)
        model.stop()
    }
    @Test func aScopeClockUpdateDoesNotReadOrEraseEdits() async throws {
        let read = SettingsReader(), model = model(read)
        model.activate(scope); try await settingsEventually { model.isFresh }
        model.editTone(bass: 4)
        model.activate(.speaker(settingsZone(state: "paused"), room: "Kitchen"))
        #expect(await read.calls.count == 1); #expect(model.tone?.bass == 4); model.stop()
    }
}
