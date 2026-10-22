import Foundation

/// Ordered occurrences in the daemon's local playlist cache, not Sonos queue slots.
public struct PlaylistContentsRequest: Equatable, Sendable {
    public static let pageSize: UInt32 = 50
    public let playlistUri: String
    public let query: String
    public let offset: UInt32
    public let expectedRevision: String?

    public init(playlistUri: String, query: String = "", offset: UInt32 = 0,
                expectedRevision: String? = nil) throws {
        let query = query.trimmingCharacters(in: .whitespacesAndNewlines)
        guard Self.source(playlistUri, kind: "playlist"), query.unicodeScalars.count <= 512,
              !query.unicodeScalars.contains(where: CharacterSet.controlCharacters.contains),
              expectedRevision.map(Self.revision) ?? (offset == 0) else {
            throw ContractError.invalid("Choose a cached playlist, a filter of at most 512 Unicode characters, and its revision when continuing.")
        }
        self.playlistUri = playlistUri; self.query = query
        self.offset = offset; self.expectedRevision = expectedRevision
    }

    public var queryItems: [URLQueryItem] {
        var items = [URLQueryItem(name: "playlist_uri", value: playlistUri),
                     URLQueryItem(name: "q", value: query),
                     URLQueryItem(name: "offset", value: String(offset)),
                     URLQueryItem(name: "limit", value: String(Self.pageSize))]
        if let expectedRevision { items.append(URLQueryItem(name: "expected_revision", value: expectedRevision)) }
        return items
    }

    static func source(_ value: String, kind: String) -> Bool {
        let parts = value.split(separator: ":", omittingEmptySubsequences: false)
        return parts.count == 3 && parts[0] == "spotify" && parts[1] == Substring(kind)
            && !parts[2].isEmpty && parts[2].utf8.count <= 128
            && parts[2].utf8.allSatisfy { (48...57).contains($0) || (65...90).contains($0) || (97...122).contains($0) }
    }
    static func revision(_ value: String) -> Bool {
        !value.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty && value.utf8.count <= 256
            && !value.unicodeScalars.contains(where: CharacterSet.controlCharacters.contains)
    }
    static func exact(_ lhs: String?, _ rhs: String?) -> Bool {
        switch (lhs, rhs) {
        case (nil, nil): true
        case (.some(let a), .some(let b)): a.utf8.elementsEqual(b.utf8)
        default: false
        }
    }
}

public struct PlaylistTrack: Decodable, Equatable, Identifiable, Sendable {
    public let position: UInt32
    public let sourceUri: String
    public let title: String
    public let subtitle: String
    public let album: String?
    public let albumUri: String?
    public let discNumber: UInt32?
    public let trackNumber: UInt32?
    public let durationSecs: UInt32?
    public let explicit: Bool
    public var id: UInt32 { position }

    func sameOccurrence(as other: Self) -> Bool {
        self == other && PlaylistContentsRequest.exact(title, other.title)
            && PlaylistContentsRequest.exact(subtitle, other.subtitle)
            && PlaylistContentsRequest.exact(album, other.album)
    }
}

public struct PlaylistContentsPage: Decodable, Equatable, Sendable {
    public enum Coverage: String, Decodable, Sendable { case complete, partial, notCached = "not_cached" }
    public let playlistUri: String
    public let name: String
    public let q: String
    public let coverage: Coverage
    public let fetchedAt: Int64?
    public let sourceTotal: UInt32?
    public let examined: UInt32
    public let cachedTotal: UInt32
    public let total: UInt32
    public let offset: UInt32
    public let limit: UInt32
    public let nextOffset: UInt32?
    public let revision: String?
    public let items: [PlaylistTrack]

    public func validated(for request: PlaylistContentsRequest) throws -> Self {
        func invalid() -> ContractError { .invalid("The cached playlist response is inconsistent. Read it again before choosing music.") }
        let count = min(request.offset < total ? total - request.offset : 0, PlaylistContentsRequest.pageSize)
        let end = UInt64(offset) + UInt64(items.count)
        guard playlistUri == request.playlistUri, PlaylistContentsRequest.exact(q, request.query),
              offset == request.offset, limit == PlaylistContentsRequest.pageSize,
              total <= cachedTotal, cachedTotal <= examined,
              !request.query.isEmpty || total == cachedTotal,
              items.count == Int(count), name.utf8.count <= 16_384,
              nextOffset == (end < UInt64(total) ? UInt32(exactly: end) : nil) else { throw invalid() }
        switch coverage {
        case .notCached:
            guard fetchedAt == nil, sourceTotal == nil, revision == nil,
                  examined == 0, cachedTotal == 0, total == 0, items.isEmpty, nextOffset == nil else { throw invalid() }
        case .complete, .partial:
            guard fetchedAt != nil, revision.map(PlaylistContentsRequest.revision) == true,
                  sourceTotal.map({ $0 >= examined && (coverage != .complete || $0 == examined) }) ?? true else { throw invalid() }
        }
        if let expected = request.expectedRevision,
           !PlaylistContentsRequest.exact(expected, revision) {
            throw ContractError.invalid("The cached playlist changed. Read its first page again; older pages were not combined.")
        }
        var previous: UInt32 = 0
        for (index, item) in items.enumerated() {
            guard item.position > previous, item.position <= examined,
                  UInt64(item.position) > UInt64(offset) + UInt64(index),
                  PlaylistContentsRequest.source(item.sourceUri, kind: "track"),
                  item.albumUri.map({ PlaylistContentsRequest.source($0, kind: "album") }) ?? true,
                  [item.title, item.subtitle, item.album ?? ""].allSatisfy({ $0.utf8.count <= 16_384 }) else { throw invalid() }
            previous = item.position
        }
        return self
    }

    public var coverageText: String {
        if coverage == .notCached {
            return "Playlist listed, but its tracks have not been cached. Sync explicitly in Spotify & library to try reading them."
        }
        let extent = sourceTotal.map(String.init) ?? "an unknown number of"
        let prefix = coverage == .complete ? "Complete cached observation" : "Partial cached observation"
        return "\(prefix): \(examined) of \(extent) source entries examined; \(cachedTotal) playable occurrences cached. Unavailable entries leave gaps in the original positions. This is not a live Spotify snapshot."
    }
    public var emptyText: String {
        if coverage == .notCached { return "Tracks not yet cached." }
        if cachedTotal > 0 { return "No cached tracks match this filter or page." }
        if coverage == .complete && examined == 0 { return "The last complete observation found an empty playlist." }
        if coverage == .complete { return "The observed playlist has no supported playable tracks." }
        return "No playable tracks in the partial observation; the full playlist may contain more music."
    }
}

extension DaemonClient {
    /// Only the existing read-only cache endpoint: no topology, Spotify sync or playback.
    public func playlistContents(_ request: PlaylistContentsRequest) async throws -> PlaylistContentsPage {
        try Task.checkCancellation()
        let page = try await get(PlaylistContentsPage.self, ["library", "playlist"], query: request.queryItems)
        try Task.checkCancellation()
        return try page.validated(for: request)
    }
}

/// Immutable handoff from a particular cached occurrence to the ordinary shared writer.
public struct PlaylistTrackSelection: Equatable, Sendable {
    public enum Action: String, CaseIterable, Sendable {
        case playNow, queueNext, queueLast
        public var title: String {
            switch self { case .playNow: "Play now"; case .queueNext: "Queue next"; case .queueLast: "Add to end of queue" }
        }
    }
    public let item: PlaylistTrack
    public let playlistName: String
    public let request: PlaylistContentsRequest
    public let zone: Zone
    public let action: Action

    public init(item: PlaylistTrack, page: PlaylistContentsPage, request: PlaylistContentsRequest,
                in zone: Zone, action: Action) throws {
        _ = try zone.validated(); _ = try page.validated(for: request)
        guard let revision = page.revision, page.items.contains(where: { $0.sameOccurrence(as: item) }) else {
            throw ContractError.invalid("Choose an occurrence from the current cached playlist page.")
        }
        self.request = try PlaylistContentsRequest(playlistUri: request.playlistUri, query: request.query,
                                                  offset: request.offset, expectedRevision: revision)
        self.item = item; self.playlistName = page.name; self.zone = zone; self.action = action
    }
    public var title: String { action.title + ": " + item.title }
    public var explanation: String {
        let effect = action == .playNow ? "Replaces this group's source and starts this track."
            : "Adds this track without starting playback or replacing the queue."
        return "\(item.title)\n\(item.subtitle)\nPlaylist: \(playlistName)\nOriginal playlist position: \(item.position)\nSource: \(item.sourceUri)\nDestination: \(zone.title) · \(zone.household)\n\n\(effect) The cached occurrence and its revision are checked again before sending. This is a playlist position, never a Sonos queue position; Spotify is not contacted or reserved by the check."
    }
    var isQueueAdd: Bool { action != .playNow }
    var allowsTrackProgress: Bool { action == .queueLast }
    var path: [String] { action == .playNow ? ["play"] : ["queue"] }

    func fields(in current: Zone) throws -> [String: JSONValue] {
        guard current.sameGroup(as: zone) else {
            throw ContractError.invalid("The selected playlist destination changed. Review its current group again.")
        }
        var fields: [String: JSONValue] = ["zone": .string(zone.id.address), "source_uri": .string(item.sourceUri)]
        if action != .playNow { fields["next"] = .bool(action == .queueNext) }
        return fields
    }
    func validate(_ page: PlaylistContentsPage) throws {
        _ = try page.validated(for: request)
        guard page.items.contains(where: { $0.sameOccurrence(as: item) }) else {
            throw ContractError.invalid("The selected cached playlist occurrence changed. Choose it again; nothing was sent.")
        }
    }
    func receipt(_ data: Data, in current: Zone?) throws -> CommandReceipt {
        guard current?.sameGroup(as: zone) == true else { throw ClientError.unreadable }
        if action == .playNow { return try NativeCommand.playSource(item.sourceUri).receipt(data, in: current) }
        let added = try DaemonClient.decode(QueueAdded.self, from: data)
        guard !added.done.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              added.first > 0, added.count > 0,
              UInt64(added.first) + UInt64(added.count) - 1 <= UInt64(UInt32.max) else { throw ClientError.unreadable }
        return CommandReceipt(title: added.done, detail: "Added \(added.count) entries at queue position \(added.first).\n"
            + (added.notes ?? []).map(\.detail).joined(separator: "\n"))
    }
}
