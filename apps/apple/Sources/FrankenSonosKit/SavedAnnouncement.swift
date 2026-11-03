import Foundation

/// A durable speech/chime recipe returned by the daemon. No generated URL,
/// host path, or audio bytes are retained in a schedule.
public struct SavedAnnouncement: Codable, Equatable, Sendable {
    public let text: String?
    public let chime: String?
    public let voice: String?
    public let rooms: [String]
    public let volume: Int?
    private let wavBase64: String?

    public init(text: String? = nil, chime: String? = nil, voice: String? = nil,
                rooms: [String] = [], volume: Int? = nil) {
        self.text = text; self.chime = chime; self.voice = voice
        self.rooms = rooms; self.volume = volume; self.wavBase64 = nil
    }

    public var targetLabel: String {
        rooms.isEmpty ? "All discovered rooms" : "Rooms: " + rooms.joined(separator: ", ")
    }

    public var summary: String {
        var parts = [text.map { "Say: " + $0 } ?? "Chime: " + (chime ?? "unknown")]
        if let voice { parts.append("Voice: " + voice) }
        parts.append("Announcement volume: \(volume ?? 35) (current policy caps apply)")
        return parts.joined(separator: "\n")
    }

    public func validated() throws -> Self {
        guard wavBase64 == nil, (text == nil) != (chime == nil),
              volume.map({ (0...100).contains($0) }) ?? true,
              rooms.allSatisfy({ room in
                  let name = room.trimmingCharacters(in: .whitespacesAndNewlines)
                  return !name.isEmpty && name.unicodeScalars.count <= 128
              }) else { throw ContractError.invalid("The scheduled announcement recipe is incomplete or unsupported.") }
        if let text {
            let speech = text.trimmingCharacters(in: .whitespacesAndNewlines)
            guard !speech.isEmpty, speech.unicodeScalars.count <= 1_000 else {
                throw ContractError.invalid("The scheduled speech is empty or too long.")
            }
            if let voice {
                let name = voice.trimmingCharacters(in: .whitespacesAndNewlines)
                guard !name.isEmpty, name.utf8.count <= 4_096, !name.hasPrefix("-"),
                      !name.unicodeScalars.contains(where: { $0.value < 32 || (127...159).contains($0.value) }) else {
                    throw ContractError.invalid("The scheduled voice is invalid.")
                }
            }
        } else {
            guard voice == nil,
                  chime.map({ ["bell", "beep", "rise"].contains($0.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()) }) == true else {
                throw ContractError.invalid("The scheduled chime is unsupported.")
            }
        }
        return self
    }

    var fields: [String: JSONValue] {
        var result: [String: JSONValue] = ["rooms": .array(rooms.map(JSONValue.string))]
        if let text { result["text"] = .string(text) }
        if let chime { result["chime"] = .string(chime) }
        if let voice { result["voice"] = .string(voice) }
        if let volume { result["volume"] = .integer(Int64(volume)) }
        return result
    }
}
