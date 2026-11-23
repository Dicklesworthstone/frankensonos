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

/// One explicit bounded page. No room or saved-library revision is needed.
public struct SpotifySearchRequest: Equatable, Sendable {
    public let query: String
    public let kind: SpotifyCatalogKind
    public let limit: UInt32
    public let offset: UInt32

    public init(query: String, kind: SpotifyCatalogKind = .track, limit: UInt32 = 10, offset: UInt32 = 0) throws {
        let trimmed = query.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !query.unicodeScalars.contains(where: { $0.properties.generalCategory == .control }),
              (1...256).contains(trimmed.utf8.count), (1...10).contains(limit) else {
            throw ContractError.invalid("Enter a Spotify search of 1–256 UTF-8 bytes without control characters; request up to 10 results.")
        }
        guard offset <= 1_000 else {
            throw ContractError.invalid("Spotify search offset must be between 0 and 1000.")
        }
        self.query = trimmed; self.kind = kind; self.limit = limit; self.offset = offset
    }

    public var queryItems: [URLQueryItem] {
        var items = [URLQueryItem(name: "q", value: query), URLQueryItem(name: "kind", value: kind.rawValue),
                     URLQueryItem(name: "limit", value: String(limit))]
        if offset != 0 { items.append(URLQueryItem(name: "offset", value: String(offset))) }
        return items
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
    /// Raw provider positions include unavailable and duplicate rows.
    public let offset: UInt32
    public let nextOffset: UInt32?

    public init(query: String, kind: SpotifyCatalogKind, items: [SpotifySearchItem], more: Bool,
                offset: UInt32 = 0, nextOffset: UInt32? = nil) {
        self.query = query; self.kind = kind; self.items = items; self.more = more
        self.offset = offset; self.nextOffset = nextOffset
    }

    private enum CodingKeys: String, CodingKey { case query, kind, items, more, offset, nextOffset }

    public init(from decoder: any Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        query = try values.decode(String.self, forKey: .query)
        kind = try values.decode(SpotifyCatalogKind.self, forKey: .kind)
        items = try values.decode([SpotifySearchItem].self, forKey: .items)
        more = try values.decode(Bool.self, forKey: .more)
        // Missing fields support an older daemon's first page. An explicit
        // malformed offset still fails, and a later request must match exactly.
        if values.contains(.offset) { offset = try values.decode(UInt32.self, forKey: .offset) }
        else { offset = 0 }
        nextOffset = try values.decodeIfPresent(UInt32.self, forKey: .nextOffset)
    }

    public func validated(for request: SpotifySearchRequest) throws -> Self {
        func label(_ text: String) -> Bool {
            text.utf8.count <= 4_096 && !text.unicodeScalars.contains(where: { $0.properties.generalCategory == .control })
        }
        guard query.utf8.elementsEqual(request.query.utf8), kind == request.kind,
              items.count <= request.limit, offset == request.offset else {
            throw ContractError.invalid("The Spotify search response does not match this query. Search again.")
        }
        if let nextOffset {
            guard more, nextOffset > offset, nextOffset <= 1_000,
                  nextOffset <= offset + request.limit, nextOffset - offset >= UInt32(items.count) else {
                throw ContractError.invalid("Spotify returned an inconsistent search continuation. Search again.")
            }
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
