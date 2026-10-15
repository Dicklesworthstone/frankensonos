import Foundation

/// A Spotify thing the person copied or shared: `spotify:album:<id>` or an open.spotify.com link. The daemon plays the
/// canonical `spotify:<kind>:<id>` form.
struct SpotifyLink: Equatable {
    static let kinds: Set<String> = ["track", "album", "playlist", "artist", "episode", "show"]

    let kind: String
    let id: String
    var uri: String { "spotify:\(kind):\(id)" }

    /// The first Spotify link in `text`, which may carry whitespace, a tracking query or a locale segment.
    static func parse(_ text: String) -> Result<SpotifyLink, SpotifyLinkError> {
        guard let match = pattern.firstMatch(in: text, range: NSRange(text.startIndex..., in: text)) else { return .failure(.notSpotify) }
        func group(_ index: Int) -> String? {
            Range(match.range(at: index), in: text).map { String(text[$0]) }
        }
        if group(5) != nil { return .failure(.shortLink) }
        guard let kind = group(1) ?? group(3), let id = group(2) ?? group(4) else { return .failure(.notSpotify) }
        guard kinds.contains(kind) else { return .failure(.unsupportedKind) }
        guard id.count == 22 else { return .failure(.cutOff) }
        return .success(SpotifyLink(kind: kind, id: id))
    }

    // Groups: 1 and 2 are the URI form, 3 and 4 the web form (after an optional intl-xx segment), 5 a spotify.link URL.
    private static let pattern = try! NSRegularExpression(
        pattern: #"spotify:([a-z]+):([A-Za-z0-9]+)|https?://open\.spotify\.com/(?:intl-[A-Za-z-]+/)?([a-z]+)/([A-Za-z0-9]+)|(https?://spotify\.link/\S*)"#)
}

enum SpotifyLinkError: Error, Equatable {
    case notSpotify, unsupportedKind, shortLink, cutOff

    var message: String {
        switch self {
        case .notSpotify: "That isn't a Spotify link."
        case .unsupportedKind: "That isn't a Spotify track or album link."
        case .shortLink: "Open the short link in Spotify first, then share the open.spotify.com one."
        case .cutOff: "That Spotify link looks cut off."
        }
    }
}
