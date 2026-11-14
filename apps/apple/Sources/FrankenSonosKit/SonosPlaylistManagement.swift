import Foundation

/// A saved Sonos catalog change, bound to one observed household and group.
/// These actions never become playback commands and never replace an existing
/// playlist by title. Two entries with equal titles remain distinct SQ: ids.
public struct SonosPlaylistMutation: Equatable, Sendable {
    public enum Operation: String, Sendable { case save, rename, remove }
    public let operation: Operation
    public let zone: Zone
    public let playlist: SonosPlaylist?
    public let newTitle: String?

    public static func save(title: String, in zone: Zone) throws -> Self {
        Self(operation: .save, zone: try zone.validated(), playlist: nil,
             newTitle: try validatedTitle(title))
    }
    public static func rename(_ playlist: SonosPlaylist, title: String, in zone: Zone) throws -> Self {
        Self(operation: .rename, zone: try zone.validated(), playlist: try playlist.validated(),
             newTitle: try validatedTitle(title))
    }
    public static func remove(_ playlist: SonosPlaylist, in zone: Zone) throws -> Self {
        Self(operation: .remove, zone: try zone.validated(), playlist: try playlist.validated(), newTitle: nil)
    }

    public static func validatedTitle(_ value: String) throws -> String {
        // Match Rust str::trim and char::is_control, including NEL whitespace
        // while preserving format characters that are not Unicode controls.
        let start = value.unicodeScalars.drop(while: { $0.properties.isWhitespace })
        let trimmed = start.reversed().drop(while: { $0.properties.isWhitespace }).reversed()
        let title = String(String.UnicodeScalarView(trimmed))
        guard !title.isEmpty, title.utf8.count <= 1_023,
              !title.unicodeScalars.contains(where: { $0.properties.generalCategory == .control }) else {
            throw ContractError.invalid("Enter a nonempty playlist title of at most 1023 UTF-8 bytes, without control characters.")
        }
        return title
    }

    public var title: String {
        switch operation {
        case .save: "Save queue as playlist"
        case .rename: "Rename saved playlist"
        case .remove: "Delete saved playlist"
        }
    }
    public var explanation: String {
        switch operation {
        case .save:
            "Creates a new saved playlist in \(zone.household) from this group's retained queue, even if radio or another source is playing. Existing playlists with this title stay separate. Playback, volume and DJ feeding continue unchanged. Sonos saves the queue when it handles the request; another controller can still edit it after this review."
        case .rename:
            "Changes only the title of \(playlist?.id ?? "") in \(zone.household). The saved tracks and current playback stay unchanged. Scenes pinned to the old playlist title will need to be saved again. Catalog changes are not undoable."
        case .remove:
            "Permanently deletes \(playlist?.id ?? "") from \(zone.household)'s saved catalog. Current playback and the group's retained queue stay unchanged. Scenes that use this saved playlist will no longer be able to load it. Catalog changes are not undoable."
        }
    }
    var path: [String] { ["sonos", "playlists", operation.rawValue] }
    func fields(revision: String?) throws -> [String: JSONValue] {
        var body: [String: JSONValue] = ["zone": .string(zone.id.address)]
        if let newTitle { body["title"] = .string(newTitle) }
        if operation != .save {
            guard let playlist, let revision, SonosPlaylistContentsRequest.validRevision(revision) else {
                throw ContractError.invalid("Read the exact saved playlist and its revision before changing it.")
            }
            body["playlist"] = .string(playlist.id)
            body["expected_revision"] = .string(revision)
        }
        return body
    }
}

/// Read-only preview data. Save uses the retained queue, not the currently
/// audible source; catalog edits use the full saved-content revision.
struct SonosPlaylistMutationObservation: Sendable {
    let catalog: [SonosPlaylist]
    let queue: SourceQueueReview?
    let source: ZoneState?
    let contents: SonosPlaylistContents?

    static func group(_ expected: Zone, in zones: [Zone]) throws -> Zone {
        guard let current = zones.first(where: { Announcement.sameGroup($0, expected) }) else {
            throw ContractError.invalid("The original group changed or is unavailable. Choose its current queue or saved catalog and review again.")
        }
        return current
    }

    static func read(_ command: SonosPlaylistMutation, client: DaemonClient) async throws -> Self {
        try Task.checkCancellation()
        _ = try group(command.zone, in: await client.zones())
        let catalog = try await client.sonosPlaylists(command.zone)
        let queue: SourceQueueReview?
        let source: ZoneState?
        let contents: SonosPlaylistContents?
        if command.operation == .save {
            source = try await client.state(command.zone)
            queue = try await SourceQueueReview.read(client, in: command.zone)
            guard (queue?.total ?? 0) > 0 else {
                throw ContractError.invalid("This group's retained queue is empty. Queue some music before saving a playlist.")
            }
            contents = nil
        } else {
            guard let expected = command.playlist,
                  catalog.filter({ $0.id.utf8.elementsEqual(expected.id.utf8) }).count == 1,
                  catalog.contains(where: { $0.sameSelection(as: expected) }) else {
                throw ContractError.invalid("The selected saved playlist changed or disappeared. Refresh its original household and review again.")
            }
            contents = try await client.sonosPlaylistContents(.init(playlist: expected, in: command.zone))
            queue = nil; source = nil
        }
        // Catalog/queue reads can be slow. Check membership after them as well.
        _ = try group(command.zone, in: await client.zones())
        try Task.checkCancellation()
        return Self(catalog: catalog, queue: queue, source: source, contents: contents)
    }

    func validate(_ current: Self, command: SonosPlaylistMutation) throws {
        if command.operation == .save {
            guard let before = queue, let after = current.queue,
                  before.zone.utf8.elementsEqual(after.zone.utf8), before.total == after.total,
                  before.revision.utf8.elementsEqual(after.revision.utf8),
                  PlaylistContentsRequest.exact(before.items.first?.uri, after.items.first?.uri) else {
                throw ContractError.invalid("The retained queue changed after review. Review its current contents before saving; nothing was sent.")
            }
            // Playback may advance while the same retained queue is saved.
            // New equal-title entries matter to the duplicate-title warning.
            let titled: ([SonosPlaylist]) -> Set<String> = { values in
                Set(values.filter { $0.title.utf8.elementsEqual((command.newTitle ?? "").utf8) }.map(\.id))
            }
            guard titled(catalog) == titled(current.catalog) else {
                throw ContractError.invalid("The household's matching playlist titles changed. Review the save again; equal titles create separate playlists.")
            }
        } else {
            guard let before = contents, let after = current.contents,
                  before.playlist.sameSelection(as: after.playlist), before.total == after.total,
                  before.revision.utf8.elementsEqual(after.revision.utf8) else {
                throw ContractError.invalid("The saved playlist changed after review. Read its current contents and review again; nothing was sent.")
            }
        }
    }
}

public struct SonosPlaylistMutationReview: Identifiable, Sendable {
    public let id = UUID()
    public let command: SonosPlaylistMutation
    public let observedAt = Date()
    let observation: SonosPlaylistMutationObservation
    public var trackCount: UInt32 { observation.queue?.total ?? observation.contents?.total ?? 0 }
    public var currentSource: String? { observation.source?.track?.displayTitle }
    public var firstTrack: String? { observation.queue?.items.first?.title ?? observation.contents?.items.first?.title }
    public var duplicateTitles: Int {
        observation.catalog.filter { $0.title.utf8.elementsEqual((command.newTitle ?? "").utf8) }.count
    }
    var body: [String: JSONValue] { get throws { try command.fields(revision: observation.contents?.revision) } }

    func receipt(_ data: Data) throws -> SonosPlaylistMutationReceipt {
        struct Reply: Decodable {
            let done: String
            let zone: String
            let playlist: SonosPlaylist
            let logged: Bool
            let operation: String?
            let changed: Bool?
        }
        let value = try DaemonClient.decode(Reply.self, from: data)
        _ = try value.playlist.validated()
        guard !value.done.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              value.done.utf8.count <= 16_384,
              value.zone.utf8.elementsEqual(command.zone.coordinatorRoom.utf8) else { throw ClientError.unreadable }
        let changed: Bool
        switch command.operation {
        case .save:
            guard value.playlist.title.utf8.elementsEqual((command.newTitle ?? "").utf8),
                  !observation.catalog.contains(where: { $0.id.utf8.elementsEqual(value.playlist.id.utf8) }) else {
                throw ClientError.unreadable
            }
            changed = true
        case .rename, .remove:
            guard let original = command.playlist, value.operation == command.operation.rawValue,
                  value.playlist.id.utf8.elementsEqual(original.id.utf8), let didChange = value.changed else {
                throw ClientError.unreadable
            }
            let expectedTitle = command.operation == .rename ? command.newTitle! : original.title
            let expectedChange = command.operation == .remove || !original.title.utf8.elementsEqual(expectedTitle.utf8)
            guard value.playlist.title.utf8.elementsEqual(expectedTitle.utf8), didChange == expectedChange else {
                throw ClientError.unreadable
            }
            changed = didChange
        }
        return SonosPlaylistMutationReceipt(done: value.done, playlist: value.playlist,
                                           changed: changed, logged: value.logged)
    }
}

public struct SonosPlaylistMutationReceipt: Equatable, Sendable {
    public let done: String
    public let playlist: SonosPlaylist
    public let changed: Bool
    public let logged: Bool
    public var detail: String {
        "\(done)\nNative playlist: \(playlist.id) · \(playlist.title)" + (logged ? "" :
            "\nThe Sonos result was confirmed, but the action log could not be written. Do not repeat the operation.")
    }
}

/// Explicit read-back is an observation, never proof that a lost request ran.
public struct SonosPlaylistCatalogRecovery: Sendable {
    public let target: RoomID
    public let catalog: [SonosPlaylist]
    public let observedAt: Date
    public let detail: String

    static func read(_ result: SonosPlaylistManagementResult, client: DaemonClient) async throws -> Self {
        let target = result.command.zone.id
        let zones = try await client.zones()
        guard let current = zones.first(where: { zone in
            zone.household.utf8.elementsEqual(target.household.utf8) &&
                zone.members.contains(where: { $0.utf8.elementsEqual(target.room.utf8) })
        }) else {
            throw ContractError.invalid("The original room \(target.address) is unavailable. This catalog result cannot be checked through another household.")
        }
        let catalog = try await client.sonosPlaylists(current)
        _ = try SonosPlaylistMutationObservation.group(current, in: await client.zones())
        try Task.checkCancellation()
        let note: String
        if result.command.operation == .save {
            let count = catalog.filter { $0.title.utf8.elementsEqual((result.command.newTitle ?? "").utf8) }.count
            note = "This household now has \(count) playlist(s) with the requested title. Equal titles can identify different playlists; a matching title does not prove that the earlier save succeeded."
        } else if let original = result.command.playlist,
                  let found = catalog.first(where: { $0.id.utf8.elementsEqual(original.id.utf8) }) {
            note = "\(original.id) is currently listed as “\(found.title)”. This is its present catalog state, not confirmation of an earlier lost reply."
        } else {
            note = "The selected native playlist ID is not currently listed. Its absence does not prove which request removed it."
        }
        return Self(target: target, catalog: catalog, observedAt: Date(), detail: note + " Nothing was repeated.")
    }
}
