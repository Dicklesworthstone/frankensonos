import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

private final class ScheduleScreenTransport: DaemonTransport, Sendable {
    let base = ScheduleTestTransport()
    let stream: AsyncThrowingStream<EventSignal, Error>
    let output: AsyncThrowingStream<EventSignal, Error>.Continuation
    init() {
        let pair = AsyncThrowingStream<EventSignal, Error>.makeStream()
        stream = pair.stream; output = pair.continuation
    }
    func send(_ request: URLRequest) async throws -> HTTPReply { try await base.send(request) }
    func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> {
        output.yield(.connected); return stream
    }
}
@MainActor private func scheduleScreenWait(_ condition: () -> Bool) async throws {
    let deadline = ContinuousClock.now.advanced(by: .seconds(3))
    while !condition() {
        guard ContinuousClock.now < deadline else { throw ContractError.invalid("Schedule screen did not settle") }
        try await Task.sleep(for: .milliseconds(5))
    }
}
@MainActor private func scheduleScreen(_ transport: ScheduleScreenTransport, waitForRooms: Bool = true) async throws -> RemoteModel {
    let model = RemoteModel(client: DaemonClient(endpoint: try DaemonEndpoint("https://music.example.test"), transport: transport))
    model.show(.schedules); model.resume()
    try await scheduleScreenWait { model.canManageSchedules && (!waitForRooms || model.selectedZone != nil) }
    return model
}

@Test func scheduleFormHasNoImplicitRoomAndProducesAllFourExactActions() throws {
    let zones = [scheduleZone(), scheduleZone("TEST-B")], scenes = [scheduleTestScene()]
    var form = ScheduleForm()
    #expect(throws: ContractError.self) { try form.draft(zones: zones, scenes: scenes) }
    form.room = scheduleOtherRoom
    #expect(try form.draft(zones: zones, scenes: scenes).action == .djStart(room: scheduleOtherRoom, mood: nil))
    form.action = .volume; form.volume = "40"
    #expect(try form.draft(zones: zones, scenes: scenes).action == .volume(room: scheduleOtherRoom, level: 40))
    form.action = .pause; form.fadeSeconds = "600"
    #expect(try form.draft(zones: zones, scenes: scenes).action == .pause(room: scheduleOtherRoom, fadeSecs: 600))
    form.action = .scene; form.scene = scenes[0].name; form.room = nil
    #expect(try form.draft(zones: [], scenes: scenes).action == .scene(scenes[0]))
    form.frequency = .once; form.once = "2028-03-12T07:30:00-04:00"
    #expect(try form.draft(zones: [], scenes: scenes).time.canonical() == "once 2028-03-12T07:30:00-04:00")
}

@Test func scheduleFormRejectsCoercionsMissingScenesAndWrongHouseholds() throws {
    var form = ScheduleForm(); form.room = scheduleOtherRoom
    #expect(throws: ContractError.self) { try form.draft(zones: [scheduleZone()], scenes: []) }
    for text in ["", "-1", "+1", "1.0", "1e1", " 1 ", "101", "１２", String(repeating: "9", count: 80)] {
        form.action = .volume; form.volume = text
        #expect(throws: ContractError.self) { try form.draft(zones: [scheduleZone("TEST-B")], scenes: []) }
    }
    form.action = .pause; form.fadeSeconds = "601"
    #expect(throws: ContractError.self) { try form.draft(zones: [scheduleZone("TEST-B")], scenes: []) }
    form.action = .scene; form.scene = "missing"
    #expect(throws: ContractError.self) { try form.draft(zones: [], scenes: [scheduleTestScene()]) }
}

@Test @MainActor func scheduleScreenCreatesEachActionAndManagesStoredDefinitions() async throws {
    let transport = ScheduleScreenTransport(), model = try await scheduleScreen(transport)
    defer { model.stop() }
    for action in ScheduleForm.Action.allCases {
        var form = ScheduleForm(); form.action = action; form.room = scheduleOtherRoom
        form.scene = scheduleTestScene().name
        model.updateScheduleForm(form); model.prepareScheduleDraft()
        try await scheduleScreenWait { model.scheduleReview != nil }
        let id = try #require(model.scheduleReview?.id)
        model.confirmSchedule(id); model.confirmSchedule(id)
        try await scheduleScreenWait { model.canManageSchedules }
        #expect(model.scheduleResult?.kind == .confirmed)
        #expect(model.scheduleResult?.receipt?.schedule.action == (action == .dj ? "dj_start" : action == .scene ? "apply_scene" : action.rawValue))
    }
    for change in [ScheduleChange.pause, .resume, .remove] {
        let row = try #require(model.schedules.first)
        model.prepareSchedule(.change(row, change))
        try await scheduleScreenWait { model.scheduleReview != nil }
        model.confirmSchedule(try #require(model.scheduleReview?.id))
        try await scheduleScreenWait { model.canManageSchedules }
        #expect(model.scheduleResult?.receipt?.removed == (change == .remove))
    }
    #expect(await transport.base.writes().count == 7)
}

@Test @MainActor func scheduleScreenWorksWithoutAnyReadableTopologyOrSelectedRoom() async throws {
    let transport = ScheduleScreenTransport()
    await transport.base.setReadFailure("/zones")
    let model = try await scheduleScreen(transport, waitForRooms: false)
    defer { model.stop() }
    #expect(model.selectedRoom == nil && !model.canPrepare && model.canManageSchedules)
    model.prepareSchedule(.change(try #require(model.schedules.first), .pause))
    try await scheduleScreenWait { model.scheduleReview != nil }
    model.confirmSchedule(try #require(model.scheduleReview?.id))
    try await scheduleScreenWait { model.scheduleResult != nil }
    #expect(model.scheduleResult?.kind == .confirmed)
    #expect(await transport.base.writes().count == 1)
}

@Test @MainActor func scheduleScreenFormSurvivesRefreshAndNeverFollowsSidebarSelection() async throws {
    let transport = ScheduleScreenTransport(), model = try await scheduleScreen(transport)
    defer { model.stop() }
    var form = ScheduleForm(); form.room = scheduleOtherRoom; form.action = .volume; form.volume = "37"
    model.updateScheduleForm(form)
    model.select(scheduleRoom); model.refreshPanel()
    model.show(.queue); model.show(.schedules)
    try await scheduleScreenWait { model.canManageSchedules }
    #expect(model.scheduleForm == form)
    model.prepareScheduleDraft()
    try await scheduleScreenWait { model.scheduleReview != nil }
    if case .add(let draft) = model.scheduleReview?.command { #expect(draft.action.room == scheduleOtherRoom) }
    else { Issue.record("Missing schedule draft") }
    model.cancelReview()
    try await scheduleScreenWait { model.canManageSchedules }
    #expect(model.scheduleForm == form)
    #expect(await transport.base.writes().isEmpty)
}

@Test @MainActor func scheduleScreenInvalidFormShowsErrorWithoutIssuingACommand() async throws {
    let transport = ScheduleScreenTransport(), model = try await scheduleScreen(transport)
    defer { model.stop() }
    model.prepareScheduleDraft()
    #expect(model.scheduleFormProblem != nil && model.scheduleReview == nil)
    #expect(await transport.base.writes().isEmpty)
}

@Test @MainActor func scheduleScreenFailedListIsNotEmptyAndClearsActionableRows() async throws {
    let transport = ScheduleScreenTransport(), model = try await scheduleScreen(transport)
    defer { model.stop() }
    let old = try #require(model.schedules.first)
    await transport.base.setReadFailure("/schedules"); model.refreshPanel()
    try await scheduleScreenWait { !model.panelLoading && model.panelProblem != nil }
    #expect(model.schedules.isEmpty && model.schedulesObservedAt == nil && !model.canManageSchedules)
    model.prepareSchedule(.change(old, .remove))
    #expect(model.scheduleReview == nil)
    await transport.base.setReadFailure(nil); await transport.base.setRows([]); model.refreshPanel()
    try await scheduleScreenWait { model.canManageSchedules }
    #expect(model.schedules.isEmpty && model.schedulesObservedAt != nil && model.panelProblem == nil)
}

@Test @MainActor func scheduleScreenSceneCatalogFailurePreservesStoredControlsAndRoomCreation() async throws {
    let transport = ScheduleScreenTransport()
    await transport.base.setReadFailure("/scenes")
    let model = try await scheduleScreen(transport)
    defer { model.stop() }
    #expect(!model.schedules.isEmpty && model.scheduleScenes.isEmpty && model.scheduleScenesProblem != nil)
    var form = ScheduleForm(); form.room = scheduleRoom
    model.updateScheduleForm(form); model.prepareScheduleDraft()
    try await scheduleScreenWait { model.scheduleReview != nil }
    model.confirmSchedule(try #require(model.scheduleReview?.id))
    try await scheduleScreenWait { model.canManageSchedules }
    #expect(model.scheduleResult?.kind == .confirmed)
}

@Test @MainActor func scheduleScreenCancelledListCannotReplaceAnotherPanel() async throws {
    let transport = ScheduleScreenTransport(), model = try await scheduleScreen(transport)
    defer { model.stop() }
    await transport.base.blockNext("GET", "/schedules"); model.refreshPanel()
    try await waitForScheduleGate(transport.base)
    model.show(.scenes)
    #expect(model.schedules.isEmpty && model.schedulesObservedAt == nil)
    await transport.base.release()
    try await scheduleScreenWait { !model.panelLoading && !model.scenes.isEmpty }
    #expect(model.panel == .scenes && model.schedulesObservedAt == nil)
}

@Test @MainActor func scheduleScreenSuspensionAndLeavingPanelCancelUnsentConfirmation() async throws {
    for suspend in [true, false] {
        let transport = ScheduleScreenTransport(), model = try await scheduleScreen(transport)
        defer { model.stop() }
        model.prepareSchedule(.change(try #require(model.schedules.first), .pause))
        try await scheduleScreenWait { model.scheduleReview != nil }
        await transport.base.blockNext("GET", "/schedules")
        model.confirmSchedule(try #require(model.scheduleReview?.id))
        try await waitForScheduleGate(transport.base)
        if suspend { model.suspend() } else { model.show(.scenes) }
        await transport.base.release()
        try await scheduleScreenWait { model.actionPhase == .idle }
        #expect(model.scheduleResult?.kind == .notSent)
        #expect(await transport.base.writes().isEmpty)
    }
}

@Test @MainActor func scheduleScreenLateReceiptKeepsStoredTargetAcrossHouseholdChanges() async throws {
    let transport = ScheduleScreenTransport(), model = try await scheduleScreen(transport)
    defer { model.stop() }
    let original = try #require(model.schedules.first)
    model.prepareSchedule(.change(original, .pause))
    try await scheduleScreenWait { model.scheduleReview != nil }
    await transport.base.blockNext("POST", "/schedules/pause")
    model.confirmSchedule(try #require(model.scheduleReview?.id))
    try await waitForScheduleGate(transport.base)
    model.select(scheduleOtherRoom)
    await transport.base.release()
    try await scheduleScreenWait { model.scheduleResult != nil && model.actionPhase == .idle }
    #expect(model.scheduleResult?.receipt?.schedule.zone == "Studio@TEST-A")
    #expect(model.scheduleResult?.kind == .confirmed && model.selectedRoom == scheduleOtherRoom)
    #expect(await transport.base.writes().count == 1)
}

@Test @MainActor func scheduleScreenPlaybackAndScheduleControlsCannotRaceEachOther() async throws {
    let transport = ScheduleScreenTransport(), model = try await scheduleScreen(transport)
    defer { model.stop() }
    model.prepareSchedule(.change(try #require(model.schedules.first), .pause))
    try await scheduleScreenWait { model.scheduleReview != nil }
    model.prepare(.pause)
    #expect(model.review == nil && !model.canPrepare)
    await transport.base.blockNext("POST", "/schedules/pause")
    model.confirmSchedule(try #require(model.scheduleReview?.id))
    try await waitForScheduleGate(transport.base)
    model.prepare(.pause); model.prepareSchedule(.change(try #require(model.schedules.first), .remove))
    #expect(model.review == nil && model.scheduleReview == nil && model.actionPhase == .confirming)
    await transport.base.release()
    try await scheduleScreenWait { model.canManageSchedules }
    model.prepare(.pause)
    try await scheduleScreenWait { model.review != nil }
    model.prepareSchedule(.change(try #require(model.schedules.first), .remove))
    #expect(model.scheduleReview == nil)
    #expect(await transport.base.writes().count == 1)
}

@Test @MainActor func scheduleScreenLostCreationAcknowledgementSurvivesRefreshWithoutReplay() async throws {
    let transport = ScheduleScreenTransport(), model = try await scheduleScreen(transport)
    defer { model.stop() }
    var form = ScheduleForm(); form.room = scheduleRoom
    model.updateScheduleForm(form); model.prepareScheduleDraft()
    try await scheduleScreenWait { model.scheduleReview != nil }
    let id = try #require(model.scheduleReview?.id)
    await transport.base.loseNextReply(); model.confirmSchedule(id)
    try await scheduleScreenWait { model.scheduleResult?.kind == .uncertain && model.canManageSchedules }
    model.confirmSchedule(id); model.refresh(); model.suspend(); model.resume()
    try await scheduleScreenWait { model.canManageSchedules }
    #expect(model.scheduleResult?.kind == .uncertain && model.scheduleReview == nil)
    #expect(model.schedules.count == 2 && model.scheduleForm == form)
    #expect(await transport.base.writes().count == 1)
}

@Test @MainActor func scheduleScreenUnrelatedTopologyEventsDoNotCancelStoreReviews() async throws {
    let transport = ScheduleScreenTransport(), model = try await scheduleScreen(transport)
    defer { model.stop() }
    model.prepareSchedule(.change(try #require(model.schedules.first), .pause))
    try await scheduleScreenWait { model.scheduleReview != nil }
    let id = try #require(model.scheduleReview?.id)
    await transport.base.setTopology([scheduleZone("TEST-B")])
    transport.output.yield(.event(ServerEvent(id: "1", kind: "topology.changed", data: "{}")))
    try await scheduleScreenWait { model.selectedZone == nil }
    #expect(model.scheduleReview?.id == id)
    model.confirmSchedule(id)
    try await scheduleScreenWait { model.scheduleResult != nil }
    #expect(model.scheduleResult?.kind == .confirmed)
}

@Test @MainActor func scheduleScreenQuietPanelDoesNotPollOrDispatchOnItsOwn() async throws {
    let transport = ScheduleScreenTransport(), model = try await scheduleScreen(transport)
    defer { model.stop() }
    try await Task.sleep(for: .milliseconds(350))
    let before = await transport.base.requests.filter { $0.url!.path == "/schedules" }.count
    try await Task.sleep(for: .milliseconds(350))
    #expect(await transport.base.requests.filter { $0.url!.path == "/schedules" }.count == before)
    #expect(await transport.base.writes().isEmpty)
}

@Test @MainActor func scheduleScreenEditingAReviewConsumesItWithoutLosingTheDraft() async throws {
    let transport = ScheduleScreenTransport(), model = try await scheduleScreen(transport)
    defer { model.stop() }
    var form = ScheduleForm(); form.room = scheduleRoom
    model.updateScheduleForm(form); model.prepareScheduleDraft()
    try await scheduleScreenWait { model.scheduleReview != nil }
    let old = try #require(model.scheduleReview?.id)
    form.time = "08:45"; model.updateScheduleForm(form)
    model.confirmSchedule(old)
    #expect(model.scheduleReview == nil && model.scheduleForm.time == "08:45")
    model.prepareScheduleDraft()
    try await scheduleScreenWait { model.scheduleReview != nil }
    model.confirmSchedule(try #require(model.scheduleReview?.id))
    try await scheduleScreenWait { model.scheduleResult != nil }
    #expect(model.scheduleResult?.receipt?.schedule.when == "weekdays 08:45")
    #expect(await transport.base.writes().count == 1)
}

@Test @MainActor func scheduleScreenAChangedStoredDefinitionRequiresAnotherReview() async throws {
    let transport = ScheduleScreenTransport(), model = try await scheduleScreen(transport)
    defer { model.stop() }
    model.prepareSchedule(.change(try #require(model.schedules.first), .remove))
    try await scheduleScreenWait { model.scheduleReview != nil }
    await transport.base.setRows([scheduleFixture(["zone": .string("Studio@TEST-B")])])
    model.confirmSchedule(try #require(model.scheduleReview?.id))
    try await scheduleScreenWait { model.scheduleResult != nil && model.canManageSchedules }
    #expect(model.scheduleResult?.kind == .notSent)
    #expect(model.schedules.first?.zone == "Studio@TEST-B")
    #expect(await transport.base.writes().isEmpty)
}
