import Foundation

// What `fsonos serve` answers on /spotify/*. Field names match crates/fsonos-api/src/spotify.rs.

struct SpotifyStatus: Decodable, Equatable {
    var configured: Bool
    var signedIn: Bool
    var reauthorize: Bool
    var library: SpotifyLibrary
    var sync: SpotifySync
    var clientID: String? = nil
    var appRedirectURI: String? = nil
    private enum CodingKeys: String, CodingKey {
        case configured, signedIn = "signed_in", reauthorize, library, sync, clientID = "client_id", appRedirectURI = "app_redirect_uri"
    }
}

struct SpotifyLibrary: Decodable, Equatable {
    var albums: Int
    var tracks: Int
    var syncedAt: Int64?
    private enum CodingKeys: String, CodingKey { case albums, tracks, syncedAt = "synced_at" }
}

struct SpotifySync: Decodable, Equatable {
    var running: Bool
    var done: Int
    var total: Int
    var error: SpotifySyncError?
}

struct SpotifySyncError: Decodable, Equatable {
    var detail: String
    var retryable: Bool
    var retryAt: Int64?
    private enum CodingKeys: String, CodingKey { case detail, retryable, retryAt = "retry_at" }
}

struct SpotifyPage<Item: Decodable>: Decodable {
    var total: Int
    var items: [Item]
}

struct SpotifyAlbum: Decodable, Identifiable, Equatable {
    var id: String
    var title: String
    var artist: String
    var year: Int?
    var tracks: Int
    var uri: String
    var artUrl: URL?
    private enum CodingKeys: String, CodingKey { case id, title, artist, year, tracks, uri, artUrl = "art_url" }
}

/// `GET /spotify/search`: matches from all of Spotify, not just the owner's library.
struct SpotifySearchResults: Decodable, Equatable {
    var albums: [SpotifyAlbum]
    var tracks: [SpotifyTrack]
    var isEmpty: Bool { albums.isEmpty && tracks.isEmpty }
}

/// A track of an album, or a liked track (which also carries its album's name and art).
struct SpotifyTrack: Decodable, Identifiable, Equatable {
    var id: String
    var title: String
    var artists: [String]
    var uri: String
    var durationSecs: Int?
    var disc: Int?
    var number: Int?
    var album: String?
    var artUrl: URL?
    private enum CodingKeys: String, CodingKey {
        case id, title, artists, uri, disc, number, album
        case durationSecs = "duration_secs", artUrl = "art_url"
    }

    var artistLine: String { artists.joined(separator: ", ") }

    /// `3:03`, or an empty string when the length is unknown.
    var durationText: String {
        guard let seconds = durationSecs, seconds > 0 else { return "" }
        return "\(seconds / 60):" + String(format: "%02d", seconds % 60)
    }
}
