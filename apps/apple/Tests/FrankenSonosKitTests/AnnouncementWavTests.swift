import Foundation
import Testing
@testable import FrankenSonosKit

// Generated PCM containers only: no recordings, paths or house identifiers.
func announcementLE(_ value: Int, _ count: Int) -> Data {
    Data((0..<count).map { UInt8(truncatingIfNeeded: value >> ($0 * 8)) })
}
func announcementChunk(_ name: String, _ bytes: Data) -> Data {
    Data(name.utf8) + announcementLE(bytes.count, 4) + bytes + (bytes.count % 2 == 1 ? Data([0]) : Data())
}
func announcementRIFF(_ chunks: [Data]) -> Data {
    let body = Data("WAVE".utf8) + chunks.reduce(Data(), +)
    return Data("RIFF".utf8) + announcementLE(body.count, 4) + body
}
func announcementFormat(rate: Int = 8_000, channels: Int = 1, extensible: Bool = false) -> Data {
    var body = announcementLE(extensible ? 0xfffe : 1, 2) + announcementLE(channels, 2)
    body += announcementLE(rate, 4) + announcementLE(rate * channels * 2, 4)
    body += announcementLE(channels * 2, 2) + announcementLE(16, 2)
    if extensible {
        body += announcementLE(22, 2) + announcementLE(16, 2) + announcementLE(channels == 1 ? 4 : 3, 4)
        body += Data([1, 0, 0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xaa, 0, 0x38, 0x9b, 0x71])
    }
    return announcementChunk("fmt ", body)
}
func announcementAudio(frames: Int = 80, rate: Int = 8_000, channels: Int = 1, extensible: Bool = false) -> Data {
    announcementRIFF([announcementFormat(rate: rate, channels: channels, extensible: extensible),
                      announcementChunk("data", Data(repeating: 0, count: frames * channels * 2))])
}

@Test func announcementWavAcceptsEveryDaemonRateAndChannelCount() throws {
    for rate in [8_000, 11_025, 12_000, 16_000, 22_050, 24_000, 32_000, 44_100, 48_000] {
        for channels in [1, 2] {
            let data = announcementAudio(frames: rate, rate: rate, channels: channels)
            let clip = try AnnouncementWav(data: data, name: "Example.wav")
            #expect(clip.sampleRate == rate && clip.channels == channels)
            #expect(clip.frames == rate && clip.duration == 1)
            #expect(clip.byteCount == data.count)
            #expect(Data(base64Encoded: clip.base64) == data)
        }
    }
}

@Test func announcementWavAcceptsExtensiblePCMAndPaddedMetadata() throws {
    let fmt = announcementFormat(rate: 48_000, channels: 2, extensible: true)
    let data = announcementRIFF([announcementChunk("JUNK", Data([1, 2, 3])), fmt,
        announcementChunk("data", Data([0, 0, 0, 0])), announcementChunk("LIST", Data([9]))])
    let clip = try AnnouncementWav(data: data)
    #expect(clip.channels == 2 && clip.frames == 1 && clip.sampleRate == 48_000)
    #expect(Data(base64Encoded: clip.base64) == data)
}

@Test func announcementWavRejectsUnsupportedPCMAndInconsistentRates() {
    // Absolute offsets of the standard 44-byte header.
    for (offset, value, count) in [(20, 3, 2), (22, 0, 2), (22, 3, 2), (24, 0, 4),
                                  (24, 96_000, 4), (28, 123, 4), (32, 4, 2), (34, 24, 2)] {
        var data = announcementAudio()
        data.replaceSubrange(offset..<(offset + count), with: announcementLE(value, count))
        #expect(throws: ContractError.self) { try AnnouncementWav(data: data) }
    }
}

@Test func announcementWavRejectsIncompleteExtensibleFormats() {
    for (offset, value, count) in [(36, 21, 2), (36, 23, 2), (38, 8, 2), (44, 3, 1), (59, 0, 1)] {
        var data = announcementAudio(extensible: true)
        data.replaceSubrange(offset..<(offset + count), with: announcementLE(value, count))
        #expect(throws: ContractError.self) { try AnnouncementWav(data: data) }
    }
    let short = announcementRIFF([announcementChunk("fmt ", announcementLE(0xfffe, 2) + Data(repeating: 0, count: 14)),
                                   announcementChunk("data", Data([0, 0]))])
    #expect(throws: ContractError.self) { try AnnouncementWav(data: short) }
}

@Test func announcementWavRejectsEmptyDuplicateAndMisorderedChunks() {
    let fmt = announcementFormat(), samples = announcementChunk("data", Data([0, 0]))
    for chunks in [[fmt], [samples, fmt], [fmt, fmt, samples], [fmt, samples, samples],
                   [fmt, announcementChunk("data", Data())],
                   [fmt, announcementChunk("data", Data([0]))]] {
        #expect(throws: ContractError.self) { try AnnouncementWav(data: announcementRIFF(chunks)) }
    }
}

@Test func announcementWavRejectsEveryTruncationAndForgedLength() {
    let valid = announcementAudio(frames: 1)
    for length in 0..<valid.count {
        #expect(throws: ContractError.self) { try AnnouncementWav(data: valid.prefix(length)) }
    }
    for offset in [4, 16, 40] {
        var data = valid
        data.replaceSubrange(offset..<(offset + 4), with: announcementLE(0xffffffff, 4))
        #expect(throws: ContractError.self) { try AnnouncementWav(data: data) }
    }
    var trailing = valid + Data([1])
    trailing.replaceSubrange(4..<8, with: announcementLE(trailing.count - 8, 4))
    #expect(throws: ContractError.self) { try AnnouncementWav(data: trailing) }
    var missingPad = announcementRIFF([announcementFormat(), announcementChunk("data", Data([0, 0])),
                                      announcementChunk("JUNK", Data([1]))])
    missingPad.removeLast()
    missingPad.replaceSubrange(4..<8, with: announcementLE(missingPad.count - 8, 4))
    #expect(throws: ContractError.self) { try AnnouncementWav(data: missingPad) }
}

@Test func announcementWavEnforcesExactDurationBoundary() throws {
    let data = announcementAudio(frames: 300 * 8_000)
    #expect(try AnnouncementWav(data: data).duration == 300)
    #expect(throws: ContractError.self) { try AnnouncementWav(data: announcementAudio(frames: 300 * 8_000 + 1)) }
}

@Test func announcementWavEnforcesByteLimitIncludingMetadata() throws {
    let fmt = announcementFormat(), samples = announcementChunk("data", Data([0, 0]))
    let filler = AnnouncementWav.maximumBytes - 12 - fmt.count - samples.count - 8
    let exact = announcementRIFF([fmt, samples, announcementChunk("JUNK", Data(repeating: 0, count: filler))])
    #expect(exact.count == AnnouncementWav.maximumBytes)
    #expect(try AnnouncementWav(data: exact).byteCount == AnnouncementWav.maximumBytes)
    #expect(throws: ContractError.self) { try AnnouncementWav(data: exact + Data([0])) }
}

@Test func announcementWavSnapshotsSlicedInputAndRetainsOnlyDisplayName() throws {
    var data = Data([9, 9]) + announcementAudio()
    let clip = try AnnouncementWav(data: data.dropFirst(2), name: "/local/private/例.wav")
    let retained = clip.base64
    data[data.count - 1] = 1
    #expect(clip.base64 == retained)
    #expect(clip.name == "例.wav")
    #expect(!clip.summary.contains("/local/private"))
}

@Test func announcementWavPayloadContainsExactBytesNotAFilePathOrOtherSource() throws {
    let data = announcementAudio(frames: 3)
    let clip = try AnnouncementWav(data: data, name: "Example.wav")
    let zone = Zone(household: "S2", coordinatorRoom: "Studio", members: ["Studio", "Hall"], transportState: "playing")
    let value = try Announcement(source: .wav(clip), rooms: ["Hall"], volume: 0, in: zone)
    let body = try value.fields(in: zone)
    #expect(body == ["rooms": .array([.string("Hall@S2")]), "volume": .integer(0), "wav_base64": .string(data.base64EncodedString())])
    let encoded = try JSONEncoder().encode(body)
    #expect(try JSONDecoder().decode([String: JSONValue].self, from: encoded) == body)
    #expect(value.explanation.contains("3 frames") && value.explanation.contains("Hall@S2"))
    #expect(!value.explanation.contains(clip.base64))
    #expect(throws: ContractError.self) { try Announcement(source: .wav(clip), rooms: [], in: zone) }
    let other = Zone(household: "S1", coordinatorRoom: "Studio", members: ["Studio", "Hall"], transportState: "playing")
    #expect(throws: ContractError.self) { try value.fields(in: other) }
}

@Test func announcementWavKeepsExistingSpeechAndChimePayloads() throws {
    let zone = Zone(household: "S1", coordinatorRoom: "Studio", members: ["Studio"], transportState: "stopped")
    let speech = try Announcement(source: .speech(" Hello "), rooms: ["Studio"], in: zone)
    let chime = try Announcement(source: .chime(.bell), rooms: ["Studio"], in: zone)
    #expect(try speech.fields(in: zone)["text"] == .string("Hello"))
    #expect(try chime.fields(in: zone)["chime"] == .string("bell"))
    #expect(try speech.fields(in: zone)["wav_base64"] == nil)
    #expect(try chime.fields(in: zone)["wav_base64"] == nil)
}

@Test func announcementWavReportPreservesPartialRestoration() throws {
    let zone = Zone(household: "S1", coordinatorRoom: "Studio", members: ["Studio"], transportState: "stopped")
    let request = try Announcement(source: .wav(AnnouncementWav(data: announcementAudio())), rooms: ["Studio"], in: zone)
    let json = #"{"done":"clip finished","clean":false,"households":[{"lead":"Studio","rooms":["Studio"],"levels":[{"room":"Studio","volume":20}],"outcome":"finished","not_restored":["Studio volume changed elsewhere"]}]}"#
    let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
    let report = try decoder.decode(AnnouncementReport.self, from: Data(json.utf8)).validated(for: request)
    #expect(!report.clean && report.detail.contains("Not restored: Studio volume changed elsewhere"))
    #expect(throws: ContractError.self) {
        try decoder.decode(AnnouncementReport.self, from: Data(json.replacingOccurrences(of: "\"clean\":false", with: "\"clean\":true").utf8)).validated(for: request)
    }
}
