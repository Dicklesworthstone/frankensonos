import Foundation
import XCTest
@testable import LiveModel

/// Opt-in: drives the app's real store against a real daemon and real speakers, on the zone named by
/// FSONOS_REAL_HOUSE_ZONE ONLY, and records how the shown play state moves after a tap. It refuses to run unless that
/// zone exists on its own, so it can never start any other room.
///
///   FSONOS_LIVE_CHECKS=1 FSONOS_REAL_HOUSE_URL=http://<daemon>:8099 FSONOS_REAL_HOUSE_ZONE=Kitchen \
///     swift test --disable-sandbox --filter RealHouseCheck
///
/// Set FSONOS_EXPECT_NO_FLICKER=1 to fail when a tap is ever followed by the opposite state.
@MainActor
final class RealHouseCheck: XCTestCase {
    func testTapDoesNotFlicker() async throws {
        let env = ProcessInfo.processInfo.environment
        guard let urlText = env["FSONOS_REAL_HOUSE_URL"], let url = URL(string: urlText), let room = env["FSONOS_REAL_HOUSE_ZONE"] else {
            throw XCTSkip("set FSONOS_REAL_HOUSE_URL and FSONOS_REAL_HOUSE_ZONE to run against a real house")
        }
        let store = LiveZoneStore(baseURL: url)
        store.start()
        defer { store.setActive(false) }
        try await LiveChecks.wait("live", seconds: 25) { store.connectionStatus == .live && !store.zones.isEmpty }
        guard let zone = store.zones.first(where: { $0.roomNames == [room] }) else {
            XCTFail("REFUSING: no zone that is exactly [\(room)]. Zones: \(store.zones.map(\.roomNames))")
            return
        }
        let id = zone.id
        func shown() -> Bool { store.zones.first(where: { $0.id == id })?.isPlaying ?? false }
        var flickered = false
        func timeline(_ label: String) async throws {
            let start = Date()
            let before = shown()
            var last = before
            var out = ["\(label): start=\(before ? "playing" : "paused")"]
            store.togglePlayback(for: id)
            let wanted = !before
            out.append("tap -> \(shown() ? "playing" : "paused")")
            while Date().timeIntervalSince(start) < 7 {
                try await Task.sleep(for: .milliseconds(20))
                let current = shown()
                if current != last {
                    out.append(String(format: "+%.2fs %@", Date().timeIntervalSince(start), current ? "playing" : "paused"))
                    if current != wanted { flickered = true }
                    last = current
                }
            }
            print("TIMELINE " + out.joined(separator: " | "))
            if let error = store.commandError { print("TIMELINE   commandError: \(error)") }
        }
        let started = shown()
        print("TIMELINE zone \(room): shown playing at start = \(started)")
        try await timeline("toggle 1")
        try await timeline("toggle 2")
        if shown() != started { try await timeline("restore") }
        print("TIMELINE final shown playing = \(shown()) (started \(started)), flickered = \(flickered)")
        if env["FSONOS_EXPECT_NO_FLICKER"] == "1" { XCTAssertFalse(flickered, "a tap was followed by the opposite state") }
    }
}
