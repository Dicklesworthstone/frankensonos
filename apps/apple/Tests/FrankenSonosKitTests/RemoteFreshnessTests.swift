import Foundation
import Testing
@testable import FrankenSonosKit

@Test @MainActor func healthySelectedRoomKeepsTimelineAndQueueLiveDuringAnotherRoomOutage() async throws {
    let room = freshZone("A"), offline = freshZone("Offline")
    let transport = FreshRoomTransport([room, offline])
    await transport.fail([offline.id])
    let model = RemoteModel(client: try freshClient(transport))
    defer { model.stop() }
    model.resume()
    try await freshnessWait { await MainActor.run { model.selectedPlaybackIsFresh && model.queue != nil } }
    #expect(model.house?.isFresh == false)
    let timeline = try #require(model.selectedTimeline)
    #expect(timeline.isFresh && timeline.canSeek)
    #expect(timeline.position(at: timeline.observedAt.addingTimeInterval(10)) == 20)
    #expect(model.house?.snapshot?.failures[offline.id] != nil)
    await transport.setVersion(2)
    try await transport.event("zone.state", room: "A")
    // Panel reads and live playback settle independently. An already pending
    // queue read can observe version 2 before the room's state batch finishes.
    try await freshnessWait { await MainActor.run {
        model.queue?.items.first?.title == "Pick 2" &&
        model.selectedState?.track?.uri == "fixture:2" && model.selectedPlaybackIsFresh
    } }
    #expect(model.selectedState?.track?.uri == "fixture:2")
    #expect(model.selectedPlaybackIsFresh && model.house?.isFresh == false)
    #expect(await transport.requests.allSatisfy { $0.httpMethod == "GET" })
}

@Test @MainActor func pendingSelectedRoomFreezesButSwitchingToSettledRoomKeepsItsClockLive() async throws {
    let a = freshZone("A"), b = freshZone("B")
    let transport = FreshRoomTransport([a, b])
    let model = RemoteModel(client: try freshClient(transport))
    defer { model.stop() }
    model.resume()
    try await freshnessWait { await MainActor.run { model.house?.isFresh == true } }
    await transport.hold("/zones/A@S2/state")
    try await transport.event("zone.state", room: "A")
    try await freshnessWait { await transport.waiting["/zones/A@S2/state"]?.count == 1 }
    try await freshnessWait { await MainActor.run { !model.selectedPlaybackIsFresh } }
    let frozen = try #require(model.selectedTimeline), now = Date()
    #expect(!frozen.canSeek)
    #expect(frozen.position(at: now) == frozen.position(at: now.addingTimeInterval(30)))
    model.select(b.id)
    #expect(model.house?.isFresh == false)
    #expect(model.selectedPlaybackIsFresh && model.selectedTimeline?.canSeek == true)
    await transport.releaseAll()
    try await freshnessWait { await MainActor.run { model.house?.isFresh == true } }
    #expect(await transport.requests.allSatisfy { $0.httpMethod == "GET" })
}

@Test @MainActor func selectedPlaybackWaitsForNewStateAfterSuspensionAndTopologyChange() async throws {
    let transport = FreshRoomTransport([freshZone("A"), freshZone("B")])
    let model = RemoteModel(client: try freshClient(transport))
    defer { model.stop() }
    model.resume()
    try await freshnessWait { await MainActor.run { model.house?.isFresh == true } }
    model.suspend()
    #expect(!model.selectedPlaybackIsFresh && model.selectedTimeline?.canSeek == false)
    try await freshnessWait { await MainActor.run { model.house?.phase == .suspended } }
    await transport.hold("/zones"); model.resume()
    try await freshnessWait { await transport.waiting["/zones"]?.count == 1 }
    #expect(!model.selectedPlaybackIsFresh)
    await transport.releaseAll()
    try await freshnessWait { await MainActor.run { model.selectedPlaybackIsFresh } }
    await transport.hold("/zones")
    try await transport.event("topology.changed")
    try await freshnessWait { await transport.waiting["/zones"]?.count == 1 }
    #expect(!model.selectedPlaybackIsFresh && model.selectedTimeline?.canSeek == false)
    await transport.releaseAll()
    try await freshnessWait { await MainActor.run { model.selectedPlaybackIsFresh } }
}
