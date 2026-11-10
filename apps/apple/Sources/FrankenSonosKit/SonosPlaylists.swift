import Foundation

/// A native saved queue belongs to its Sonos household, independently of the
/// Spotify library. Only the daemon reads or supplies its renderer metadata.
public struct SonosPlaylist: Codable, Equatable, Identifiable, Sendable {
    public let id: String
    public let title: String
    public let artUri: String?

    public func validated() throws -> Self {
        guard id.hasPrefix("SQ:"), id.count > 3, id.utf8.count <= 1_024,
              id.dropFirst(3).utf8.allSatisfy({ (48...57).contains($0) }),
              !title.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              title.utf8.count <= 16_384 else { throw ClientError.unreadable }
        return self
    }

    func sameSelection(as other: Self) -> Bool {
        id.utf8.elementsEqual(other.id.utf8) && title.utf8.elementsEqual(other.title.utf8)
    }
}

public enum SonosPlaylistAction: String, CaseIterable, Sendable {
    case play, next, append
    public var title: String {
        switch self { case .play: "Play now"; case .next: "Queue next"; case .append: "Add to end of queue" }
    }
}

/// Retains the exact catalog choice and destination for the shared one-use
/// command review. Another household may have the same SQ: id and title.
public struct SonosPlaylistSelection: Equatable, Sendable {
    public let playlist: SonosPlaylist
    public let zone: Zone
    public let action: SonosPlaylistAction

    public init(_ playlist: SonosPlaylist, in zone: Zone, action: SonosPlaylistAction) throws {
        self.playlist = try playlist.validated(); self.zone = try zone.validated(); self.action = action
    }

    public var title: String { "\(action.title): \(playlist.title)" }
    public var explanation: String {
        let source = "Uses “\(playlist.title)” from the saved Sonos playlists in \(zone.household). "
        if action == .play {
            return source + "Replaces this group’s queue with the playlist’s current contents and starts playback. The previous queue cannot be recovered through ordinary undo. Playback volume caps apply."
        }
        return source + (action == .next
            ? "Inserts after the current queue track, or appends when a queue track is not selected. "
            : "Appends to the end of the current queue. ") + "Does not start or resume playback."
    }

    var path: [String] { action == .play ? ["sonos", "playlists", "play"] : ["queue"] }
    func fields(in current: Zone) throws -> [String: JSONValue] {
        _ = try current.validated()
        guard zone.sameGroup(as: current) else {
            throw ContractError.invalid("The saved playlist’s destination group changed. Choose it again in the intended household.")
        }
        var body: [String: JSONValue] = ["zone": .string(zone.id.address), "playlist": .string(playlist.id)]
        if action != .play { body["next"] = .bool(action == .next) }
        return body
    }

    func validate(_ current: [SonosPlaylist], in zone: Zone) throws {
        _ = try fields(in: zone)
        let matches = current.filter { $0.id.utf8.elementsEqual(playlist.id.utf8) }
        guard matches.count == 1, let found = matches.first, playlist.sameSelection(as: found) else {
            throw ContractError.invalid("The saved Sonos playlist changed, disappeared or is ambiguous. Refresh the catalog and review it again.")
        }
        _ = try found.validated()
    }

    func receipt(_ data: Data, in current: Zone?) throws -> CommandReceipt {
        if let current { _ = try fields(in: current) }
        if action == .play {
            let report = try DaemonClient.decode(Outcome.self, from: data)
            guard !report.done.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { throw ClientError.unreadable }
            return CommandReceipt(title: report.done, detail: report.detail)
        }
        struct Added: Decodable {
            let done: String
            let zone: String
            let first: UInt32
            let count: UInt32
            let playingQueue: Bool
            let notes: [Note]?
        }
        let report = try DaemonClient.decode(Added.self, from: data)
        guard !report.done.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              report.zone.utf8.elementsEqual(zone.coordinatorRoom.utf8), report.first > 0, report.count > 0,
              UInt64(report.first) + UInt64(report.count) - 1 <= UInt64(UInt32.max) else { throw ClientError.unreadable }
        let detail = "Added \(report.count) entries from “\(playlist.title)” at position \(report.first). " +
            (report.playingQueue ? "Playback was not started or restarted."
                : "The current source was left unchanged; the group is not playing its queue.")
        return CommandReceipt(title: report.done, detail: ([detail] + (report.notes ?? []).map(\.detail)).joined(separator: "\n"))
    }
}

public struct SonosPlaylistContentsRequest: Equatable, Sendable {
    public static let pageSize: UInt32 = 50
    public let zone: Zone
    public let playlist: SonosPlaylist
    public let offset: UInt32
    public let expectedRevision: String?

    public init(playlist: SonosPlaylist, in zone: Zone, offset: UInt32 = 0, expectedRevision: String? = nil) throws {
        self.playlist = try playlist.validated(); self.zone = try zone.validated()
        guard offset <= UInt32.max - Self.pageSize,
              expectedRevision.map(Self.validRevision) ?? (offset == 0) else {
            throw ContractError.invalid("Continue saved playlist contents with the exact preceding revision.")
        }
        self.offset = offset; self.expectedRevision = expectedRevision
    }

    static func validRevision(_ value: String) -> Bool {
        !value.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty && value.utf8.count <= 4_096
            && !value.unicodeScalars.contains(where: CharacterSet.controlCharacters.contains)
    }

    public var queryItems: [URLQueryItem] {
        var items = [URLQueryItem(name: "zone", value: zone.id.address),
                     URLQueryItem(name: "playlist", value: playlist.id),
                     URLQueryItem(name: "offset", value: String(offset)),
                     URLQueryItem(name: "limit", value: String(Self.pageSize))]
        if let expectedRevision { items.append(URLQueryItem(name: "expected_revision", value: expectedRevision)) }
        return items
    }
}

public struct SonosPlaylistItem: Decodable, Equatable, Identifiable, Sendable {
    public let position: UInt32
    public let title: String
    public let creator: String?
    public let album: String?
    public let durationSecs: UInt32?
    public let uri: String?
    public var id: UInt32 { position }
}

public struct SonosPlaylistContents: Decodable, Equatable, Sendable {
    public let zone: String
    public let playlist: SonosPlaylist
    public let revision: String
    public let total: UInt32
    public let offset: UInt32
    public let nextOffset: UInt32?
    public let items: [SonosPlaylistItem]

    public func validated(for request: SonosPlaylistContentsRequest) throws -> Self {
        _ = try playlist.validated()
        let available = total > offset ? total - offset : 0
        let end = UInt64(offset) + UInt64(items.count)
        guard zone.utf8.elementsEqual(request.zone.coordinatorRoom.utf8), playlist.sameSelection(as: request.playlist),
              offset == request.offset, SonosPlaylistContentsRequest.validRevision(revision),
              request.expectedRevision.map({ $0.utf8.elementsEqual(revision.utf8) }) ?? true,
              items.count <= Int(min(available, SonosPlaylistContentsRequest.pageSize)),
              !items.isEmpty || available == 0,
              nextOffset == (end < UInt64(total) ? UInt32(exactly: end) : nil) else {
            throw ContractError.invalid("The saved playlist changed or returned an inconsistent page. Read its catalog and first page again.")
        }
        for (index, item) in items.enumerated() {
            guard UInt64(item.position) == UInt64(offset) + UInt64(index) + 1,
                  [item.title, item.creator ?? "", item.album ?? "", item.uri ?? ""].allSatisfy({ $0.utf8.count <= 65_536 }) else {
                throw ClientError.unreadable
            }
        }
        return self
    }
}

extension DaemonClient {
    public func sonosPlaylists(_ zone: Zone) async throws -> [SonosPlaylist] {
        _ = try zone.validated()
        try Task.checkCancellation()
        let values = try await get([SonosPlaylist].self, ["sonos", "playlists"],
            query: [URLQueryItem(name: "zone", value: zone.id.address)])
        try Task.checkCancellation()
        var ids = Set<String>()
        for value in values {
            _ = try value.validated()
            guard ids.insert(value.id).inserted else { throw ClientError.unreadable }
        }
        return values
    }

    public func sonosPlaylistContents(_ request: SonosPlaylistContentsRequest) async throws -> SonosPlaylistContents {
        try Task.checkCancellation()
        let page = try await get(SonosPlaylistContents.self, ["sonos", "playlists", "contents"], query: request.queryItems)
        try Task.checkCancellation()
        return try page.validated(for: request)
    }
}
