import Foundation

/// Standing taste belongs to the daemon, not a room or a Spotify account write.
public enum DJPreferenceKey: String, CaseIterable, Codable, Sendable {
    case energy, explicit
    case favorGenres = "favor.genres", favorArtists = "favor.artists"
    case favorEras = "favor.eras", favorMoods = "favor.moods"
    case avoidGenres = "avoid.genres", avoidArtists = "avoid.artists"
    case avoidEras = "avoid.eras", avoidMoods = "avoid.moods"
    case pinArtists = "pin.artists", pinAlbums = "pin.albums", pinTracks = "pin.tracks"
    case banArtists = "ban.artists", banAlbums = "ban.albums", banTracks = "ban.tracks"

    public var isList: Bool { self != .energy && self != .explicit }
    public var group: String { String(rawValue.split(separator: ".")[0]) }
    public var category: String { String(rawValue.split(separator: ".").last!) }
    public var title: String { rawValue.replacingOccurrences(of: ".", with: " ").capitalized }
    public static var lists: [Self] { allCases.filter(\.isList) }
}

public enum DJPreferencesError: Error, LocalizedError, Equatable, Sendable {
    case invalid(String)
    public var errorDescription: String? { switch self { case .invalid(let message): message } }
}

/// Require the complete read DTO. An unavailable/malformed file is not a reset
/// to defaults. Unknown future fields are never echoed in a preference write.
public struct DJPreferences: Codable, Equatable, Sendable {
    public let energy: Int?
    public let explicit: Bool
    public let favor: Taste
    public let avoid: Taste
    public let pin: Items
    public let ban: Items

    public struct Taste: Codable, Equatable, Sendable {
        public let genres: [String]
        public let artists: [String]
        public let eras: [String]
        public let moods: [String]
    }
    public struct Items: Codable, Equatable, Sendable {
        public let artists: [String]
        public let albums: [String]
        public let tracks: [String]
    }
    private enum CodingKeys: String, CodingKey { case energy, explicit, favor, avoid, pin, ban }

    public init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        guard values.contains(.energy) else {
            throw DJPreferencesError.invalid("The DJ energy setting is missing. Refresh preferences.")
        }
        energy = try values.decodeIfPresent(Int.self, forKey: .energy)
        explicit = try values.decode(Bool.self, forKey: .explicit)
        favor = try values.decode(Taste.self, forKey: .favor)
        avoid = try values.decode(Taste.self, forKey: .avoid)
        pin = try values.decode(Items.self, forKey: .pin)
        ban = try values.decode(Items.self, forKey: .ban)
        try validate()
    }

    public func encode(to encoder: Encoder) throws {
        var values = encoder.container(keyedBy: CodingKeys.self)
        try values.encode(energy, forKey: .energy)
        try values.encode(explicit, forKey: .explicit)
        try values.encode(favor, forKey: .favor)
        try values.encode(avoid, forKey: .avoid)
        try values.encode(pin, forKey: .pin)
        try values.encode(ban, forKey: .ban)
    }

    public func validate() throws {
        guard energy.map({ (0...100).contains($0) }) ?? true else {
            throw DJPreferencesError.invalid("The DJ energy setting is outside 0–100.")
        }
        for key in DJPreferenceKey.lists {
            let entries = entries(for: key)
            guard entries.count <= 5_000, entries.allSatisfy({
                !$0.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty && $0.unicodeScalars.count <= 256
            }) else { throw DJPreferencesError.invalid("The \(key.title.lowercased()) list is incomplete or too large.") }
        }
    }

    public func entries(for key: DJPreferenceKey) -> [String] {
        switch key {
        case .favorGenres: favor.genres
        case .favorArtists: favor.artists
        case .favorEras: favor.eras
        case .favorMoods: favor.moods
        case .avoidGenres: avoid.genres
        case .avoidArtists: avoid.artists
        case .avoidEras: avoid.eras
        case .avoidMoods: avoid.moods
        case .pinArtists: pin.artists
        case .pinAlbums: pin.albums
        case .pinTracks: pin.tracks
        case .banArtists: ban.artists
        case .banAlbums: ban.albums
        case .banTracks: ban.tracks
        case .energy, .explicit: []
        }
    }
}

/// Exactly one public POST /dj/preferences operation. List removal always
/// names an observed item; there is deliberately no implicit clear-all action.
public struct DJPreferenceChange: Equatable, Encodable, Sendable {
    public let key: DJPreferenceKey
    public let value: String?
    public let unset: Bool

    public init(key: DJPreferenceKey, value: String? = nil, unset: Bool = false) throws {
        self.key = key; self.unset = unset
        if !key.isList && unset { self.value = nil; return }
        guard let text = value?.trimmingCharacters(in: .whitespacesAndNewlines),
              !text.isEmpty, text.unicodeScalars.count <= 256,
              !text.unicodeScalars.contains(where: CharacterSet.controlCharacters.contains) else {
            throw DJPreferencesError.invalid("Enter one value of 1–256 characters. Commas remain part of a name.")
        }
        if key == .energy {
            guard text.utf8.allSatisfy({ (48...57).contains($0) }), let number = Int(text), (0...100).contains(number) else {
                throw DJPreferencesError.invalid("Energy must be a whole number from 0 to 100.")
            }
            self.value = String(number)
        } else if key == .explicit {
            guard ["true", "false"].contains(text) else {
                throw DJPreferencesError.invalid("Choose whether explicit tracks are allowed.")
            }
            self.value = text
        } else { self.value = text }
    }

    public var title: String {
        if key == .energy { return unset ? "Follow time-of-day energy" : "Set standing energy to \(value!)" }
        if key == .explicit { return unset || value == "false" ? "Keep explicit tracks out" : "Allow explicit tracks" }
        return "\(unset ? "Remove" : "Add") “\(value!)” \(unset ? "from" : "to") \(key.title.lowercased())"
    }
    public static let scope = "Every group's DJ on this daemon"
    public static let explanation = "Applies from future DJ picks, not music already queued. Temporary group steering takes precedence. This does not skip, start, or restart playback. Speaker Undo does not revert standing taste; remove or change the preference to reverse it."

    public func validate(against observed: DJPreferences) throws {
        try observed.validate()
        if key.isList && unset && !observed.entries(for: key).contains(value!) {
            throw DJPreferencesError.invalid("That preference is no longer in the observed list. Refresh and choose it again.")
        }
    }
}

/// Capture the complete observed baseline so a retained native control cannot
/// silently apply a change on top of a newer preference document.
public struct DJPreferenceCommand: Equatable, Sendable {
    public let change: DJPreferenceChange
    public let before: DJPreferences
    public init(change: DJPreferenceChange, before: DJPreferences) throws {
        try change.validate(against: before)
        self.change = change; self.before = before
    }
    public func validate(_ current: DJPreferences) throws {
        try change.validate(against: current)
        guard before == current else {
            throw DJPreferencesError.invalid("Standing taste changed since this selection. Refresh and review the new preferences. Nothing was sent.")
        }
    }
}

public struct DJPreferenceReview: Identifiable, Sendable {
    public let id = UUID()
    public let command: DJPreferenceCommand
    public let observedAt: Date
    init(command: DJPreferenceCommand, observedAt: Date) { self.command = command; self.observedAt = observedAt }
}

public struct DJPreferenceReceipt: Decodable, Equatable, Sendable {
    public let done: String
    public let changed: Bool
    public let preferences: DJPreferences

    public func validated(for command: DJPreferenceCommand) throws -> Self {
        try preferences.validate()
        guard !done.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
            throw DJPreferencesError.invalid("The preference save reply is incomplete.")
        }
        let change = command.change
        let confirms: Bool
        switch change.key {
        case .energy: confirms = preferences.energy == (change.unset ? nil : Int(change.value!))
        case .explicit: confirms = preferences.explicit == (!change.unset && change.value == "true")
        default:
            let items = preferences.entries(for: change.key)
            // Adds retain the supplied spelling; no-op adds may match the
            // daemon's normalization. Do not reimplement its music matching.
            confirms = change.unset ? !items.contains(change.value!)
                : items.contains(change.value!) || (!changed && preferences == command.before)
        }
        guard confirms, changed || preferences == command.before else {
            throw DJPreferencesError.invalid("The preference save did not confirm the reviewed change. Read current taste; do not repeat the save.")
        }
        return self
    }
}
