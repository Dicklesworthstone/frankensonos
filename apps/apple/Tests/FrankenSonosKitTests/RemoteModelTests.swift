import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

private actor ScreenLayout {
    var changed = false
    func expand() { changed = true }
}

private final class ScreenTransport: DaemonTransport, Sendable {
    let base = ReviewTransport()
    let layout = ScreenLayout()
    let stream: AsyncThrowingStream<EventSignal, Error>
    let output: AsyncThrowingStream<EventSignal, Error>.Continuation
    init() {
        let pair = AsyncThrowingStream<EventSignal, Error>.makeStream()
        stream = pair.stream; output = pair.continuation
    }
    func send(_ request: URLRequest) async throws -> HTTPReply {
        let reply = try await base.send(request)
        guard await layout.changed else { return reply }
        if request.httpMethod == "POST" {
            // Match a cancellation-aware network transport for the regroup test.
            try Task.checkCancellation()
            return reply
        }
        let path = request.url!.path
        let expanded = Zone(household: "TEST-A", coordinatorRoom: "Studio", members: ["Studio", "Hall", "Deck"], transportState: "playing")
        let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
        if path == "/zones" {
            return HTTPReply(data: try encoder.encode([expanded, reviewZone(house: "TEST-B")]), status: 200)
        }
        if path.hasSuffix("/state") {
            var object = try JSONSerialization.jsonObject(with: reply.data) as! [String: Any]
            if (object["zone"] as? [String: Any])?["household"] as? String == "TEST-A" {
                object["zone"] = try JSONSerialization.jsonObject(with: encoder.encode(expanded))
                return HTTPReply(data: try JSONSerialization.data(withJSONObject: object), status: 200)
            }
        }
        return reply
    }
    func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> {
        output.yield(.connected); return stream
    }
}

@MainActor private func screenEventually(_ condition: () -> Bool) async throws {
    let deadline = ContinuousClock.now.advanced(by: .seconds(3))
    while !condition() {
        guard ContinuousClock.now < deadline else { throw ContractError.invalid("Screen state did not settle") }
        try await Task.sleep(for: .milliseconds(5))
    }
}
@MainActor private func screen(_ transport: ScreenTransport) async throws -> RemoteModel {
    let client = DaemonClient(endpoint: try DaemonEndpoint("https://music.example.test"), transport: transport)
    let model = RemoteModel(client: client)
    model.resume()
    try await screenEventually { model.selectedZone != nil && !model.panelLoading && model.queue != nil }
    return model
}

@Test @MainActor func screenReviewAndConfirmationUseTheProductionSingleShotPath() async throws {
    let transport = ScreenTransport(), model = try await screen(transport)
    defer { model.stop() }
    model.prepare(.pause)
    try await screenEventually { model.review != nil }
    #expect(await transport.base.writes().isEmpty)
    let review = try #require(model.review)
    model.confirm(review.id); model.confirm(review.id)
    try await screenEventually { model.result != nil && model.actionPhase == .idle }
    #expect(model.result?.kind == .confirmed)
    #expect(model.result?.target == RoomID(household: "TEST-A", room: "Studio"))
    #expect(await transport.base.writes().count == 1)
    model.refresh()
    try await Task.sleep(for: .milliseconds(30))
    #expect(await transport.base.writes().count == 1)
}

@Test @MainActor func screenCancellationDrainsBeforeAnotherReviewAndDoesNotRetarget() async throws {
    let transport = ScreenTransport(), model = try await screen(transport)
    defer { model.stop() }
    await transport.base.blockNext("/zones/Studio@TEST-A/state")
    model.prepare(.next)
    try await waitForReviewGate(transport.base)
    model.select(RoomID(household: "TEST-B", room: "Studio"))
    #expect(model.actionPhase == .preparing)
    model.prepare(.pause)
    await transport.base.release()
    try await screenEventually { model.actionPhase == .idle }
    #expect(model.review == nil)
    #expect(await transport.base.writes().isEmpty)
    model.prepare(.pause)
    try await screenEventually { model.review != nil }
    #expect(model.review?.zone.household == "TEST-B")
}

@Test @MainActor func screenLateReceiptKeepsOriginalTargetAfterRoomSelectionChanges() async throws {
    let transport = ScreenTransport(), model = try await screen(transport)
    defer { model.stop() }
    model.prepare(.pause)
    try await screenEventually { model.review != nil }
    let review = try #require(model.review)
    await transport.base.blockNext("/pause")
    model.confirm(review.id)
    try await waitForReviewGate(transport.base)
    model.select(RoomID(household: "TEST-B", room: "Studio"))
    #expect(model.actionPhase == .confirming)
    await transport.base.release()
    try await screenEventually { model.actionPhase == .idle && model.result != nil }
    #expect(model.result?.kind == .confirmed)
    #expect(model.result?.target.household == "TEST-A")
    #expect(model.selectedRoom?.household == "TEST-B")
    #expect(await transport.base.writes().count == 1)
}

@Test @MainActor func screenSuspendDuringConfirmationReadPreventsASend() async throws {
    let transport = ScreenTransport(), model = try await screen(transport)
    defer { model.stop() }
    model.prepare(.next)
    try await screenEventually { model.review != nil }
    let id = try #require(model.review?.id)
    await transport.base.blockNext("/zones/Studio@TEST-A/state")
    model.confirm(id)
    try await waitForReviewGate(transport.base)
    model.suspend(); model.confirm(id)
    await transport.base.release()
    try await screenEventually { model.actionPhase == .idle }
    #expect(model.result?.kind == .notSent)
    #expect(!model.canPrepare)
    #expect(await transport.base.writes().isEmpty)
    model.resume()
    try await screenEventually { model.canPrepare }
    model.prepare(.pause)
    try await screenEventually { model.review != nil }
}

@Test @MainActor func screenLostAcknowledgementSurvivesRefreshAndNeverReplays() async throws {
    let transport = ScreenTransport(), model = try await screen(transport)
    defer { model.stop() }
    model.prepare(.next)
    try await screenEventually { model.review != nil }
    let id = try #require(model.review?.id)
    await transport.base.loseWrite()
    model.confirm(id)
    try await screenEventually { model.result?.kind == .uncertain }
    model.refresh(); model.confirm(id)
    model.suspend(); model.resume()
    try await Task.sleep(for: .milliseconds(350))
    #expect(model.result?.kind == .uncertain)
    #expect(model.review == nil)
    #expect(await transport.base.writes().count == 1)
}

@Test @MainActor func screenCancelledQueueReadCannotPopulateAnotherPanel() async throws {
    let transport = ScreenTransport(), model = try await screen(transport)
    defer { model.stop() }
    await transport.base.blockNext("/zones/Studio@TEST-A/queue")
    model.refreshPanel()
    try await waitForReviewGate(transport.base)
    model.show(.favorites)
    #expect(model.queue == nil && model.favorites.isEmpty)
    await transport.base.release()
    try await screenEventually { !model.panelLoading && !model.favorites.isEmpty }
    #expect(model.panel == .favorites)
    #expect(model.queue == nil)
    #expect(model.favorites.first?.id == "FV:1")
}

@Test @MainActor func screenPanelFailureDoesNotDisableReachableRoomControls() async throws {
    let transport = ScreenTransport(), model = try await screen(transport)
    defer { model.stop() }
    // The controlled transport has no library route: this fails as a real
    // read error, while /zones and /state remain available for playback.
    model.search("Test music")
    try await screenEventually { model.panelProblem != nil }
    #expect(model.canPrepare)
    model.prepare(.pause)
    try await screenEventually { model.review != nil }
    model.confirm(try #require(model.review?.id))
    try await screenEventually { model.result?.kind == .confirmed }
    #expect(await transport.base.writes().count == 1)
}

@Test @MainActor func screenStoppedSessionDoesNotReconnectOrQueueOfflineCommands() async throws {
    let transport = ScreenTransport(), model = try await screen(transport)
    model.prepare(.pause)
    try await screenEventually { model.review != nil }
    let id = try #require(model.review?.id)
    model.stop(); model.resume(); model.confirm(id); model.prepare(.next)
    try await Task.sleep(for: .milliseconds(50))
    #expect(!model.active && !model.canPrepare)
    #expect(model.review == nil)
    #expect(await transport.base.writes().isEmpty)
}

@Test @MainActor func screenRejectsAButtonFromAnObsoleteHouseholdView() async throws {
    let transport = ScreenTransport(), model = try await screen(transport)
    defer { model.stop() }
    let previous = try #require(model.selectedZone)
    model.select(RoomID(household: "TEST-B", room: "Studio"))
    model.prepare(.volume(30, room: "Hall"), in: previous)
    try await Task.sleep(for: .milliseconds(30))
    #expect(model.review == nil && model.actionPhase == .idle)
    #expect(await transport.base.writes().isEmpty)
}

@Test @MainActor func screenMemberReceiptNamesTheMemberNotItsCoordinator() async throws {
    let transport = ScreenTransport(), model = try await screen(transport)
    defer { model.stop() }
    model.prepare(.volume(25, room: "Hall"))
    try await screenEventually { model.review != nil }
    model.confirm(try #require(model.review?.id))
    try await screenEventually { model.result != nil }
    #expect(model.result?.target == RoomID(household: "TEST-A", room: "Hall"))
    #expect(model.result?.detail.contains("Capped at 25") == true)
}

@Test @MainActor func screenQueueFavoriteAndDJActionsAreActuallyConnected() async throws {
    let transport = ScreenTransport(), model = try await screen(transport)
    defer { model.stop() }
    let queue = try #require(model.queue)
    let item = try #require(queue.items.first)
    let revision = try #require(queue.revision)
    model.prepare(.queueRemove(item, revision: revision))
    try await screenEventually { model.review != nil }
    model.confirm(try #require(model.review?.id))
    try await screenEventually { model.actionPhase == .idle && model.result != nil }
    #expect(model.result?.detail.contains("cannot be undone") == true)

    model.show(.favorites)
    try await screenEventually { !model.favorites.isEmpty && !model.panelLoading }
    model.prepare(.playFavorite(try #require(model.favorites.first?.id)))
    try await screenEventually { model.review != nil }
    #expect(model.review?.favorite?.title == "Test station")
    model.confirm(try #require(model.review?.id))
    try await screenEventually { model.actionPhase == .idle }

    model.show(.dj)
    try await screenEventually { model.dj != nil && !model.panelLoading }
    // The test daemon has no mood catalog route. Status and stop/feedback
    // controls remain available, rather than discarding the whole DJ view.
    #expect(model.panelProblem?.contains("Mood catalog unavailable") == true)
    model.prepare(.djFeedback(like: true))
    try await screenEventually { model.review != nil }
    model.confirm(try #require(model.review?.id))
    try await screenEventually { model.actionPhase == .idle }
    #expect(model.result?.title == "Feedback recorded")
    #expect(await transport.base.writes().map { $0.url?.path } == ["/queue/remove", "/play/favorite", "/dj/feedback"])
}

@Test @MainActor func screenAdapterObservesPublishedChangesWithoutOwningTheSession() async throws {
    let transport = ScreenTransport(), model = try await screen(transport)
    defer { model.stop() }
    var notifications = 0
    model.onChange = { notifications += 1 }
    model.prepare(.pause)
    try await screenEventually { model.review != nil }
    #expect(notifications > 1)
    model.cancelReview()
    #expect(model.review == nil && notifications > 2)
}


@Test @MainActor func screenGroupingEventDoesNotCancelItsOwnAcknowledgement() async throws {
    let transport = ScreenTransport(), model = try await screen(transport)
    defer { model.stop() }
    model.prepare(.party)
    try await screenEventually { model.review != nil }
    await transport.base.blockNext("/party")
    model.confirm(try #require(model.review?.id))
    try await waitForReviewGate(transport.base)
    await transport.layout.expand()
    transport.output.yield(.event(ServerEvent(id: "1", kind: "topology.changed", data: "{}")))
    try await screenEventually { model.selectedZone?.members.count == 3 }
    #expect(model.actionPhase == .confirming)
    await transport.base.release()
    try await screenEventually { model.result != nil && model.actionPhase == .idle }
    #expect(model.result?.kind == .confirmed)
    #expect(await transport.base.writes().count == 1)
}

@Test @MainActor func screenTimelineAndArtworkFollowTheSelectedGroupWithoutCommands() async throws {
    let transport = ScreenTransport(), model = try await screen(transport)
    defer { model.stop() }
    try await screenEventually { model.house?.isFresh == true }
    let timeline = try #require(model.selectedTimeline)
    #expect(timeline.duration == 240 && timeline.canSeek)
    #expect(timeline.position(at: timeline.observedAt.addingTimeInterval(10)) == 22)
    let firstArt = try #require(model.artworkURL)
    #expect(URLComponents(url: firstArt, resolvingAgainstBaseURL: false)?.queryItems?
        .first(where: { $0.name == "zone" })?.value == "Studio@TEST-A")
    model.select(RoomID(household: "TEST-B", room: "Studio"))
    #expect(model.artworkURL != firstArt)
    model.suspend()
    #expect(model.selectedTimeline?.canSeek == false)
    let stale = try #require(model.selectedTimeline)
    #expect(stale.position(at: stale.observedAt.addingTimeInterval(10)) ==
            stale.position(at: stale.observedAt.addingTimeInterval(600)))
    #expect(await transport.base.writes().isEmpty)
}
