import Foundation

/// Explicit catalog categories, shared with the daemon's read-only search.
public enum SpotifyCatalogKind: String, Codable, CaseIterable, Sendable {
    case track, album, artist, playlist, show, episode

    public var title: String {
        switch self {
        case .track: "Tracks"
        case .album: "Albums"
        case .artist: "Artists"
        case .playlist: "Playlists"
        case .show: "Shows"
        case .episode: "Episodes"
        }
    }

    func contains(_ uri: String) -> Bool {
        let prefix = "spotify:\(rawValue):"
        guard uri.hasPrefix(prefix) else { return false }
        let id = uri.utf8.dropFirst(prefix.utf8.count)
        return id.count == 22 && id.allSatisfy {
            (65...90).contains($0) || (97...122).contains($0) || (48...57).contains($0)
        }
    }
}

/// A single bounded first page. No room or saved-library revision is needed.
public struct SpotifySearchRequest: Equatable, Sendable {
    public let query: String
    public let kind: SpotifyCatalogKind
    public let limit: UInt32

    public init(query: String, kind: SpotifyCatalogKind = .track, limit: UInt32 = 10) throws {
        let trimmed = query.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !query.unicodeScalars.contains(where: { $0.properties.generalCategory == .control }),
              (1...256).contains(trimmed.utf8.count), (1...10).contains(limit) else {
            throw ContractError.invalid("Enter a Spotify search of 1–256 UTF-8 bytes without control characters; request up to 10 results.")
        }
        self.query = trimmed; self.kind = kind; self.limit = limit
    }

    public var queryItems: [URLQueryItem] {
        [URLQueryItem(name: "q", value: query), URLQueryItem(name: "kind", value: kind.rawValue),
         URLQueryItem(name: "limit", value: String(limit))]
    }
}

public struct SpotifySearchItem: Decodable, Equatable, Identifiable, Sendable {
    public let sourceUri: String
    public let title: String
    public let subtitle: String?
    public var id: String { sourceUri }
}

public struct SpotifySearchPage: Decodable, Equatable, Sendable {
    public let query: String
    public let kind: SpotifyCatalogKind
    public let items: [SpotifySearchItem]
    public let more: Bool

    public func validated(for request: SpotifySearchRequest) throws -> Self {
        func label(_ text: String) -> Bool {
            text.utf8.count <= 4_096 && !text.unicodeScalars.contains(where: { $0.properties.generalCategory == .control })
        }
        guard query.utf8.elementsEqual(request.query.utf8), kind == request.kind,
              items.count <= request.limit else {
            throw ContractError.invalid("The Spotify search response does not match this query. Search again.")
        }
        var seen = Set<String>()
        for item in items {
            guard kind.contains(item.sourceUri), seen.insert(item.sourceUri).inserted,
                  !item.title.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
                  label(item.title), item.subtitle.map(label) != false else {
                throw ContractError.invalid("Spotify search returned an invalid source or metadata. Search again.")
            }
        }
        return self
    }
}

extension DaemonClient {
    /// Search contacts only the selected daemon. Credentials and Spotify I/O
    /// stay there; no source is played, queued, or added to the saved library.
    public func searchSpotify(_ request: SpotifySearchRequest) async throws -> SpotifySearchPage {
        try Task.checkCancellation()
        // The catalog worker has a 45-second deadline; ordinary local reads
        // keep their shorter default, while this request can await that result.
        let page = try await get(SpotifySearchPage.self, ["spotify", "search"], query: request.queryItems, timeout: 60)
        try Task.checkCancellation()
        return try page.validated(for: request)
    }
}

public struct SpotifySearchObservation: Identifiable, Sendable {
    public let id = UUID()
    public let request: SpotifySearchRequest
    public let page: SpotifySearchPage
    public let receivedAt: Date
}

/// Choosing a result creates a command for the ordinary one-use review. The
/// catalog is not queried again at confirmation, so changing result order
/// cannot replace the explicitly chosen canonical source.
public struct SpotifySearchSelection: Sendable {
    public enum Action: CaseIterable, Sendable { case playNow, queueNext, queueLast }
    public let item: SpotifySearchItem
    public let zone: Zone
    public let action: Action
    public var command: NativeCommand {
        switch action {
        case .playNow: .playSource(item.sourceUri)
        case .queueNext: .queueAdd(item.sourceUri, next: true)
        case .queueLast: .queueAdd(item.sourceUri, next: false)
        }
    }
}
