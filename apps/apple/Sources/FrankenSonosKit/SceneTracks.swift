import Foundation

extension Scene {
    /// One authored occurrence. Equal source URIs may deliberately appear more
    /// than once, with distinct display titles, in the scene's saved order.
    public struct Track: Codable, Equatable, Sendable {
        public let sourceUri: String
        public let title: String?

        public init(sourceUri: String, title: String? = nil) {
            self.sourceUri = sourceUri; self.title = title
        }

        public static func == (left: Self, right: Self) -> Bool {
            guard left.sourceUri.utf8.elementsEqual(right.sourceUri.utf8) else { return false }
            switch (left.title, right.title) {
            case (nil, nil): return true
            case (.some(let a), .some(let b)): return a.utf8.elementsEqual(b.utf8)
            default: return false
            }
        }

        func validate() throws {
            let parts = sourceUri.split(separator: ":", omittingEmptySubsequences: false)
            guard parts.count == 3, parts[0] == "spotify", ["track", "episode"].contains(String(parts[1])),
                  parts[2].utf8.count == 22,
                  parts[2].utf8.allSatisfy({ (48...57).contains($0) || (65...90).contains($0) || (97...122).contains($0) }) else {
                throw ContractError.invalid("A saved scene selection needs canonical Spotify tracks or episodes with complete 22-character IDs.")
            }
            if let title {
                // Match Rust char counts and Cc controls, retaining valid XML
                // whitespace and the exact title bytes that review displays.
                guard (1...256).contains(title.unicodeScalars.count),
                      title.unicodeScalars.contains(where: { !$0.properties.isWhitespace }),
                      !title.unicodeScalars.contains(where: {
                          $0.properties.generalCategory == .control && ![9, 10, 13].contains($0.value)
                      }) else {
                    throw ContractError.invalid("A saved scene track title must contain 1 to 256 display characters.")
                }
            }
        }
    }
}

extension Scene.Source {
    func validateTracks() throws {
        guard kind == "tracks", let tracks, (1...10).contains(tracks.count),
              name == nil, uri == nil, mood == nil, direction == nil, queue == nil, playlist == nil else {
            throw ContractError.invalid("An ordered scene source needs 1 to 10 tracks or episodes and no other music source fields.")
        }
        for track in tracks { try track.validate() }
    }

    /// Shared by the saved-scene panel, command review and schedule review.
    /// Include identities as well as titles so repeated occurrences stay clear.
    public var tracksSummary: String? {
        guard kind == "tracks", let tracks, !tracks.isEmpty else { return nil }
        let entries = tracks.enumerated().map { index, track in
            let label = track.title.map { "\($0) [\(track.sourceUri)]" } ?? track.sourceUri
            return "\(index + 1). \(label)"
        }
        return "\(tracks.count) selected tracks in order:\n" + entries.joined(separator: "\n")
    }

    public var tracksRecallNote: String {
        "Recall replaces the group’s queue with this exact selection, preserving order and repeats with shuffle and repeat off. Bounded queue undo requires a complete verified scene on unchanged groups; inspect the result and undo preview."
    }
}
