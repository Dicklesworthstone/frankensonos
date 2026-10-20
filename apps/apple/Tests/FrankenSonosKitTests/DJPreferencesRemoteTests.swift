import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

private actor TasteScreenTransport: DaemonTransport {
    nonisolated let stream: AsyncThrowingStream<EventSignal, Error>
    private let feed: AsyncThrowingStream<EventSignal, Error>.Continuation
    var preferencesReads = 0
    var writes: [URLRequest] = []
    var energy: Int?
    var rooms = false
    var holdingRead = false
    var holdingWrite = false
    var lostReply = false
    private var waiter: CheckedContinuation<Void, Never>?
    init() {
        let pair = AsyncThrowingStream<EventSignal, Error>.makeStream()
        stream = pair.stream; feed = pair.continuation
    }
    func configure(rooms: Bool = false, holdRead: Bool = false, holdWrite: Bool = false, lost: Bool = false) {
        self.rooms = rooms; holdingRead = holdRead; holdingWrite = holdWrite; lostReply = lost
    }
    func release() { waiter?.resume(); waiter = nil; holdingRead = false; holdingWrite = false }
    func topology() { feed.yield(.event(ServerEvent(id: "1", kind: "topology.changed", data: "{}"))) }
    private func zone() -> [String: Any] {
        ["household": "Fixture", "coordinator_room": "Room", "members": ["Room"], "transport_state": "paused"]
    }
    private func prefs() -> [String: Any] {
        ["energy": energy.map { $0 as Any } ?? NSNull(), "explicit": false,
         "favor": ["genres": [], "artists": [], "eras": [], "moods": []],
         "avoid": ["genres": [], "artists": [], "eras": [], "moods": []],
         "pin": ["artists": [], "albums": [], "tracks": []],
         "ban": ["artists": [], "albums": [], "tracks": []]]
    }
    func send(_ request: URLRequest) async throws -> HTTPReply {
        let path = request.url!.path
        var response: Any
        if request.httpMethod == "POST" {
            writes.append(request)
            if holdingWrite { await withCheckedContinuation { waiter = $0 } }
            if lostReply { throw URLError(.networkConnectionLost) }
            if path == "/dj/preferences" {
                let body = try #require(JSONSerialization.jsonObject(with: #require(request.httpBody)) as? [String: Any])
                energy = Int(body["value"] as? String ?? "")
                response = ["done": "Preference saved", "changed": true, "preferences": prefs()]
            } else { response = ["done": "Paused", "changed": true] }
        } else if path == "/dj/preferences" {
            preferencesReads += 1
            if holdingRead { await withCheckedContinuation { waiter = $0 } }
            response = prefs()
        } else if path == "/zones" { response = rooms ? [zone()] : [] }
        else if path.hasSuffix("/state") {
            response = ["zone": zone(), "transport_state": "paused", "volume": 20, "mute": false, "group_volume": 20]
        } else if path.hasSuffix("/queue") { response = ["total": 0, "items": [], "revision": "fixture"] }
        else { return HTTPReply(data: Data("{\"code\":\"UNAVAILABLE\",\"detail\":\"Not in fixture\"}".utf8), status: 503) }
        return HTTPReply(data: try JSONSerialization.data(withJSONObject: response), status: 200)
    }
    nonisolated func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> { stream }
}

@MainActor private func tasteScreenWait(_ condition: () async -> Bool) async throws {
    for _ in 0..<500 { if await condition() { return }; try await Task.sleep(for: .milliseconds(5)) }
    Issue.record("Native preference screen condition did not finish")
}
@MainActor private func tasteScreen(_ transport: TasteScreenTransport) throws -> RemoteModel {
    RemoteModel(client: DaemonClient(endpoint: try DaemonEndpoint("https://daemon.example"), transport: transport))
}
@MainActor private func prepareTaste(_ model: RemoteModel) throws {
    model.preferences.prepare(try DJPreferenceChange(key: .energy, value: "30"),
                              from: #require(model.preferences.editor.observation))
}

@Suite("Standing taste in the real native model") @MainActor struct DJPreferencesRemoteTests {
    @Test func remoteStartupLeavesCollapsedTasteLazyAndItWorksWithoutRooms() async throws {
        let transport = TasteScreenTransport(), model = try tasteScreen(transport)
        model.resume(); try await tasteScreenWait { model.house?.snapshot != nil }
        #expect(await transport.preferencesReads == 0)
        #expect(model.selectedZone == nil)
        model.preferences.resume(); try await tasteScreenWait { model.preferences.canPrepare }
        try prepareTaste(model); try await tasteScreenWait { model.preferences.review != nil }
        model.preferences.confirm(try #require(model.preferences.review).id)
        try await tasteScreenWait { model.preferences.result != nil && !model.preferences.isEngaged }
        #expect(model.preferences.result?.kind == .confirmed)
        #expect(await transport.writes.map { $0.url!.path } == ["/dj/preferences"])
        model.stop()
    }
    @Test func pendingTasteReviewExcludesPlaybackScheduleAndUndoActions() async throws {
        let transport = TasteScreenTransport(); await transport.configure(rooms: true)
        let model = try tasteScreen(transport); model.resume()
        try await tasteScreenWait { model.selectedState != nil }
        model.preferences.resume(); try await tasteScreenWait { model.preferences.canPrepare }
        try prepareTaste(model); try await tasteScreenWait { model.preferences.review != nil }
        #expect(!model.canPrepare && !model.canSchedule)
        model.prepare(.pause); model.prepareUndo()
        #expect(model.actionPhase == .idle && model.review == nil && model.undoReview == nil)
        #expect(await transport.writes.isEmpty)
        model.preferences.cancel(); try await tasteScreenWait { model.canPrepare }
        model.stop()
    }
    @Test func aPlaybackReviewBlocksTasteAndItsCancellationDrainsBeforeTasteCanBegin() async throws {
        let transport = TasteScreenTransport(); await transport.configure(rooms: true)
        let model = try tasteScreen(transport); model.resume()
        try await tasteScreenWait { model.selectedState != nil }
        model.preferences.resume(); try await tasteScreenWait { model.preferences.canPrepare }
        model.prepare(.pause); try await tasteScreenWait { model.review != nil && model.actionPhase == .idle }
        #expect(!model.preferences.canPrepare)
        try prepareTaste(model); #expect(model.preferences.review == nil)
        model.cancelReview()
        try prepareTaste(model); try await tasteScreenWait { model.preferences.review != nil }
        #expect(model.preferences.result == nil)
        #expect(await transport.writes.isEmpty)
        model.stop()
    }
    @Test func discoveringARoomDoesNotCancelADaemonWidePreferenceReview() async throws {
        let transport = TasteScreenTransport(), model = try tasteScreen(transport)
        model.resume(); try await tasteScreenWait { model.house?.snapshot != nil }
        model.preferences.resume(); try await tasteScreenWait { model.preferences.canPrepare }
        try prepareTaste(model); try await tasteScreenWait { model.preferences.review != nil }
        let review = try #require(model.preferences.review)
        await transport.configure(rooms: true); await transport.topology()
        try await tasteScreenWait { model.selectedZone != nil }
        #expect(model.preferences.review?.id == review.id)
        model.preferences.confirm(review.id)
        try await tasteScreenWait { model.preferences.result != nil }
        #expect(model.preferences.result?.kind == .confirmed)
        #expect(await transport.writes.count == 1)
        model.stop()
    }
    @Test func parentSuspensionCancelsUnsentTasteAndRetainsTheSharedBusySlotUntilDrain() async throws {
        let transport = TasteScreenTransport(), model = try tasteScreen(transport)
        model.resume(); model.preferences.resume(); try await tasteScreenWait { model.preferences.canPrepare }
        await transport.configure(holdRead: true)
        try prepareTaste(model); try await tasteScreenWait { await transport.preferencesReads >= 2 }
        model.suspend(); #expect(model.preferences.isEngaged && !model.canSchedule)
        await transport.release(); try await tasteScreenWait { !model.preferences.isEngaged }
        #expect(model.preferences.review == nil)
        #expect(await transport.writes.isEmpty)
        model.stop()
    }
    @Test func lateSaveFailureKeepsItsPreferenceAttributionAfterTheRemoteStops() async throws {
        let transport = TasteScreenTransport(), model = try tasteScreen(transport)
        model.resume(); model.preferences.resume(); try await tasteScreenWait { model.preferences.canPrepare }
        try prepareTaste(model); try await tasteScreenWait { model.preferences.review != nil }
        await transport.configure(holdWrite: true, lost: true)
        model.preferences.confirm(try #require(model.preferences.review).id)
        try await tasteScreenWait { await transport.writes.count == 1 }
        model.stop(); await transport.release()
        try await tasteScreenWait { model.preferences.result != nil && !model.preferences.isEngaged }
        #expect(model.preferences.result?.kind == .uncertain)
        #expect(model.preferences.result?.change.key == .energy)
        model.resume(); model.preferences.resume(); model.preferences.refresh()
        #expect(!model.active && !model.preferences.active)
        #expect(await transport.writes.count == 1)
    }
    @Test func leavingYourDJDiscardsOnlyUnsentPreferenceReview() async throws {
        let transport = TasteScreenTransport(), model = try tasteScreen(transport)
        model.resume(); model.show(.dj); model.preferences.resume()
        try await tasteScreenWait { model.preferences.canPrepare }
        try prepareTaste(model); try await tasteScreenWait { model.preferences.review != nil }
        model.show(.queue); try await tasteScreenWait { !model.preferences.isEngaged }
        #expect(!model.preferences.active && model.preferences.review == nil)
        #expect(await transport.writes.isEmpty)
        model.stop()
    }
}
