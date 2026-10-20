import Foundation

/// A complete, immutable upload, not a path the daemon can open. Match the
/// daemon's announce/clip.rs PCM contract before allocating the base64 request.
public struct AnnouncementWav: Equatable, Sendable {
    public static let maximumBytes = 16 * 1_024 * 1_024
    public static let maximumSeconds = 300
    public let name: String
    public let byteCount: Int
    public let sampleRate: Int
    public let channels: Int
    public let frames: Int
    public var duration: TimeInterval { Double(frames) / Double(sampleRate) }
    private let bytes: Data

    public init(data: Data, name: String = "Audio clip") throws {
        guard data.count <= Self.maximumBytes else {
            throw ContractError.invalid("The WAV exceeds the 16 MiB announcement limit.")
        }
        // Validate the same detached bytes that will later be uploaded.
        let data = Data(Array(data))
        func invalid(_ reason: String) -> ContractError {
            .invalid("Not a playable announcement WAV: \(reason). Choose mono or stereo 16-bit PCM, at most 16 MiB and five minutes.")
        }
        // Data slices can have a nonzero startIndex. Every offset below is
        // relative to the supplied container, never an absolute Data index.
        func byte(_ offset: Int) -> Int { Int(data[data.startIndex + offset]) }
        func u16(_ offset: Int) -> Int { byte(offset) | (byte(offset + 1) << 8) }
        func u32(_ offset: Int) -> Int { u16(offset) | (u16(offset + 2) << 16) }
        func isTag(_ offset: Int, _ tag: String) -> Bool {
            data[(data.startIndex + offset)..<(data.startIndex + offset + 4)].elementsEqual(tag.utf8)
        }
        guard data.count >= 12, isTag(0, "RIFF"), isTag(8, "WAVE") else { throw invalid("no RIFF/WAVE header") }
        guard u32(4) + 8 == data.count else { throw invalid("RIFF length does not match the complete file") }
        var format: (rate: Int, channels: Int, align: Int)?
        var audioBytes: Int?
        var offset = 12
        while offset + 8 <= data.count {
            let length = u32(offset + 4)
            let body = offset + 8
            // Compare before adding a caller-supplied chunk length.
            guard length <= data.count - body else { throw invalid("truncated chunk") }
            let end = body + length
            if isTag(offset, "fmt ") {
                guard format == nil, length >= 16 else { throw invalid("duplicate or short fmt chunk") }
                switch u16(body) {
                case 1: break
                case 0xfffe:
                    let pcm: [UInt8] = [1, 0, 0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xaa, 0, 0x38, 0x9b, 0x71]
                    guard length >= 40, u16(body + 16) >= 22, u16(body + 16) + 18 <= length,
                          u16(body + 18) == 16,
                          data[(data.startIndex + body + 24)..<(data.startIndex + body + 40)].elementsEqual(pcm) else {
                        throw invalid("extensible format is not 16-bit PCM")
                    }
                default: throw invalid("not PCM")
                }
                let channels = u16(body + 2), rate = u32(body + 4), align = u16(body + 12)
                guard [1, 2].contains(channels), u16(body + 14) == 16 else {
                    throw invalid("expected mono or stereo 16-bit PCM")
                }
                guard [8_000, 11_025, 12_000, 16_000, 22_050, 24_000, 32_000, 44_100, 48_000].contains(rate) else {
                    throw invalid("unsupported sample rate")
                }
                guard align == channels * 2, u32(body + 8) == rate * align else {
                    throw invalid("inconsistent PCM block alignment or byte rate")
                }
                format = (rate, channels, align)
            } else if isTag(offset, "data") {
                guard let format else { throw invalid("data before fmt") }
                guard audioBytes == nil, length > 0, length.isMultiple(of: format.align) else {
                    throw invalid("empty, duplicate, or unaligned PCM data")
                }
                audioBytes = length
            }
            offset = end + (length & 1)
            guard offset <= data.count else { throw invalid("missing chunk padding") }
        }
        guard offset == data.count else { throw invalid("truncated chunk header") }
        guard let format, let audioBytes else { throw invalid("missing fmt or data chunk") }
        guard audioBytes <= Self.maximumSeconds * format.rate * format.align else {
            throw invalid("announcement exceeds five minutes")
        }
        // A display name never enters the wire body. Keep only the last path
        // component, and do not allow a large label to inflate the review.
        let label = (name as NSString).lastPathComponent.trimmingCharacters(in: .whitespacesAndNewlines)
        self.name = label.isEmpty ? "Audio clip" : String(label.prefix(255))
        byteCount = data.count; sampleRate = format.rate; channels = format.channels
        frames = audioBytes / format.align
        bytes = data
    }

    var base64: String { bytes.base64EncodedString() }
    public var summary: String {
        "\(name) · \(frames) frames at \(sampleRate) Hz · \(channels == 1 ? "mono" : "stereo") · \(byteCount) bytes"
    }
}
