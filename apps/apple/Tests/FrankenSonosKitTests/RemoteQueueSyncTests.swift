import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import Testing
@testable import FrankenSonosKit

private actor QueueSyncTransport: DaemonTransport {
    var requests: [URLRequest] = []
    var revision = 1
    var order = ["A", "B", "C"]
    var holding = false, blocked = false
    var inFlight = 0, peak = 0
    private var waiter: CheckedContinuation<Void, Never>?
    nonisolated let stream: AsyncThrowingStream<EventSignal, Error>
    nonisolated let output: AsyncThrowingStream<EventSignal, Error>.Continuation
    init() {
        let pair = AsyncThrowingStream<EventSignal, Error>.makeStream()
        stream = pair.stream; output = pair.continuation
    }
    func hold() { holding = true }
    func release() { blocked = false; waiter?.resume(); waiter = nil }
    func count(_ suffix: String) -> Int { requests.filter { $0.url!.path.hasSuffix(suffix) }.count }
    var writes: Int { requests.filter { $0.httpMethod == "POST" }.count }
    func reorder(count: Int = 1) {
        order = ["A", "C", "B"]; revision += 1
        // No zone.state: current track, length, playback and all levels match.
        for _ in 0..<count {
            output.yield(.event(ServerEvent(id: nil, kind: "action.logged",
                data: "{\"intent\":\"move_in_queue: Studio\",\"decision\":\"allow\"}")))
        }
    }
    private func zone() -> [String: Any] {
        ["household": "TEST-Q", "coordinator_room": "Studio", "members": ["Studio"], "transport_state": "playing"]
    }
    func send(_ request: URLRequest) async throws -> HTTPReply {
        requests.append(request)
        if request.httpMethod == "POST" {
            guard request.url!.path == "/pause" else { throw ContractError.invalid("Unexpected mutation") }
            throw URLError(.networkConnectionLost)
        }
        let path = request.url!.path
        let value: Any
        if path == "/zones" { value = [zone()] }
        else if path == "/zones/Studio@TEST-Q/state" {
            value = ["zone": zone(), "transport_state": "playing", "volume": 20, "mute": false,
                     "group_volume": 20, "track": ["uri": "https://media.example.test/A", "title": "A",
                                                   "duration_secs": 120, "position_secs": 10, "queue_position": 1]]
        } else if path == "/zones/Studio@TEST-Q/queue" {
            inFlight += 1; peak = max(peak, inFlight)
            defer { inFlight -= 1 }
            let items: [[String: Any]] = order.enumerated().map { index, name in
                ["position": index + 1, "title": name, "uri": "https://media.example.test/\(name)"]
            }
            let data = try JSONSerialization.data(withJSONObject: ["total": order.count, "items": items,
                                                                   "playing": 1, "revision": "revision-\(revision)"])
            if holding {
                holding = false; blocked = true
                await withCheckedContinuation { waiter = $0 }
            }
            return HTTPReply(data: data, status: 200)
        } else { throw ContractError.invalid("Unexpected queue sync route: \(path)") }
        return HTTPReply(data: try JSONSerialization.data(withJSONObject: value), status: 200)
    }
    nonisolated func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> {
        output.yield(.connected); return stream
    }
}
@MainActor private func queueSyncEventually(_ condition: () async -> Bool) async throws {
    let end = ContinuousClock.now.advanced(by: .seconds(5))
    while !(await condition()) {
        guard ContinuousClock.now < end else { throw ContractError.invalid("Native queue sync did not settle") }
        try await Task.sleep(for: .milliseconds(5))
    }
}
@MainActor private func queueSyncScreen(_ transport: QueueSyncTransport) async throws -> RemoteModel {
    let model = RemoteModel(client: DaemonClient(endpoint: try DaemonEndpoint("https://daemon.example.test"), transport: transport))
    model.resume()
    do {
        try await queueSyncEventually {
            model.selectedPlaybackIsFresh && (model.house?.panelRevision ?? 0) > 0 &&
                model.queue?.revision == "revision-1" && !model.panelLoading
        }
        return model
    } catch { model.stop(); throw error }
}

@Test @MainActor func nativeQueueReorderingRefreshesWithoutAPlaybackOrLengthChange() async throws {
    let transport = QueueSyncTransport(), model = try await queueSyncScreen(transport)
    defer { model.stop() }
    let state = model.selectedState
    let reads = await transport.count("/state")
    await transport.reorder()
    try await queueSyncEventually { model.queue?.revision == "revision-2" && !model.panelLoading }
    #expect(model.queue?.items.map(\.title) == ["A", "C", "B"])
    #expect(model.selectedState == state && model.queue?.total == 3 && model.queue?.playing == 1)
    #expect(await transport.count("/state") == reads)
    #expect(await transport.writes == 0)
    #expect(model.result == nil && model.review == nil)
}

@Test @MainActor func nativeQueueReorderingDuringAReadIsCoalescedAndNotLost() async throws {
    let transport = QueueSyncTransport(), model = try await queueSyncScreen(transport)
    defer { model.stop() }
    let before = await transport.count("/queue")
    let revision = try #require(model.house?.panelRevision)
    await transport.hold(); model.refreshPanel()
    try await queueSyncEventually { await transport.blocked }
    await transport.reorder(count: 100)
    try await queueSyncEventually { (model.house?.panelRevision ?? 0) > revision }
    #expect(await transport.count("/queue") == before + 1)
    await transport.release()
    try await queueSyncEventually { model.queue?.revision == "revision-2" && !model.panelLoading }
    #expect(await transport.count("/queue") == before + 2)
    #expect(await transport.peak == 1)
    #expect(await transport.writes == 0)
}

@Test @MainActor func nativeQueueEventCannotAcknowledgeOrRepeatAnUncertainCommand() async throws {
    let transport = QueueSyncTransport(), model = try await queueSyncScreen(transport)
    defer { model.stop() }
    model.prepare(.pause)
    try await queueSyncEventually { model.review != nil && model.actionPhase == .idle }
    let reviewed = try #require(model.review?.id)
    model.confirm(reviewed)
    try await queueSyncEventually { model.result?.kind == .uncertain && model.actionPhase == .idle && model.selectedPlaybackIsFresh && !model.panelLoading }
    let outcome = try #require(model.result?.id)
    await transport.reorder()
    try await queueSyncEventually { model.queue?.revision == "revision-2" && !model.panelLoading }
    model.confirm(reviewed)
    #expect(model.result?.id == outcome && model.result?.kind == .uncertain)
    #expect(model.review == nil && model.result?.target.household == "TEST-Q")
    #expect(await transport.writes == 1)
}
