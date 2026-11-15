import Foundation
import Testing
@testable import FrankenSonosKit

private func rendererState(_ status: String? = nil) -> ZoneState {
    let zone = Zone(household: "TEST-A", coordinatorRoom: "Studio", members: ["Studio"], transportState: "playing")
    return ZoneState(zone: zone, transportState: "playing", volume: 20, mute: false,
                     groupVolume: 20, track: nil, shuffle: false, repeat: "off", transportStatus: status)
}

@Test(arguments: [false, true]) func absentRendererStatusStaysUnknown(null: Bool) throws {
    let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
    var body = try JSONSerialization.jsonObject(with: encoder.encode(rendererState())) as! [String: Any]
    if null { body["transport_status"] = NSNull() } else { body.removeValue(forKey: "transport_status") }
    let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
    let value = try decoder.decode(ZoneState.self, from: JSONSerialization.data(withJSONObject: body))
    _ = try value.validated(for: value.zone)
    #expect(value.transportStatus == nil && !value.hasPlaybackError)
    #expect(value.playbackLabel == "Playing")
}

@Test func rendererFailureIsIndependentOfTransportAndRetiresPlaybackConsent() throws {
    let healthy = rendererState("OK"), failed = rendererState("ERROR_OCCURRED")
    _ = try failed.validated(for: healthy.zone)
    #expect(failed.isPlaying && failed.hasPlaybackError)
    #expect(failed.playbackLabel == "Playback error")
    #expect(!healthy.samePlayback(as: failed))
    #expect(throws: ContractError.self) { try NativeCommand.resume.validate(before: healthy, current: failed) }
    try NativeCommand.volume(25, room: nil).validate(before: healthy, current: failed)
}

@Test func rendererStatusRoundTripsAndUnknownReportsStayVisible() throws {
    let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
    let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
    for status in ["OK", "ERROR_OCCURRED", "CUSTOM_STATUS"] {
        let value = try decoder.decode(ZoneState.self, from: encoder.encode(rendererState(status)))
        #expect(value.transportStatus == status)
        if status == "CUSTOM_STATUS" { #expect(value.playbackLabel.contains(status)) }
    }
    #expect(!rendererState("OK").samePlayback(as: rendererState()))
}

@Test(arguments: ["", " \n", String(repeating: "x", count: 4_097)])
func malformedRendererStatusCannotSupplyPlaybackEvidence(_ status: String) {
    let value = rendererState(status)
    #expect(throws: ContractError.self) { try value.validated(for: value.zone) }
}
