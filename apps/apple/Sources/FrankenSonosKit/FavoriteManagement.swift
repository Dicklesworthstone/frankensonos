import Foundation

/// One My Sonos catalog edit, bound to the reviewed household and group.
/// Saving selects the actual current track or station on the daemon.
public struct FavoriteMutation: Equatable, Sendable {
    public enum Operation: String, Sendable { case save, rename, remove }
    public let operation: Operation
    public let zone: Zone
    public let favorite: Favorite?
    public let newTitle: String?

    public static func save(title: String, in zone: Zone) throws -> Self {
        Self(operation: .save, zone: try zone.validated(), favorite: nil,
             newTitle: try validatedTitle(title))
    }
    public static func rename(_ favorite: Favorite, title: String, in zone: Zone) throws -> Self {
        Self(operation: .rename, zone: try zone.validated(), favorite: try favorite.validated(),
             newTitle: try validatedTitle(title))
    }
    public static func remove(_ favorite: Favorite, in zone: Zone) throws -> Self {
        Self(operation: .remove, zone: try zone.validated(), favorite: try favorite.validated(), newTitle: nil)
    }

    static func trimmed(_ value: String) -> String {
        let start = value.unicodeScalars.drop(while: { $0.properties.isWhitespace })
        let scalars = start.reversed().drop(while: { $0.properties.isWhitespace }).reversed()
        return String(String.UnicodeScalarView(scalars))
    }

    public static func validatedTitle(_ value: String) throws -> String {
        // Match the daemon's Rust whitespace and Unicode control categories.
        let title = trimmed(value)
        guard !title.isEmpty, title.utf8.count <= 1_023,
              !title.unicodeScalars.contains(where: { $0.properties.generalCategory == .control }) else {
            throw ContractError.invalid("Enter a nonempty favorite title of at most 1023 UTF-8 bytes, without control characters.")
        }
        return title
    }

    static func validRevision(_ value: String) -> Bool {
        !trimmed(value).isEmpty && value.utf8.count <= 4_096
            && !value.unicodeScalars.contains(where: { $0.properties.generalCategory == .control })
    }

    public var title: String {
        switch operation {
        case .save: "Save current music as favorite"
        case .rename: "Rename Sonos favorite"
        case .remove: "Delete Sonos favorite"
        }
    }
    public var explanation: String {
        switch operation {
        case .save:
            "Saves the reviewed current track or station in \(zone.household)'s My Sonos favorites, using its native metadata. The source is checked again before saving. Existing favorites with the same title remain separate. Playback, queue, volume and DJ feeding continue."
        case .rename:
            "Changes the title of \(favorite?.id ?? "") in \(zone.household)'s My Sonos favorites. Its source and current playback stay unchanged. Catalog changes are not undoable."
        case .remove:
            "Permanently deletes \(favorite?.id ?? "") from \(zone.household)'s My Sonos favorites. Existing playing or queued copies remain available. Catalog changes are not undoable."
        }
    }
    var path: [String] { ["favorites", operation.rawValue] }
    func fields(revision: String?) throws -> [String: JSONValue] {
        guard let revision, Self.validRevision(revision) else {
            throw ContractError.invalid("Read the exact current source or favorite and its revision before saving a change.")
        }
        var body: [String: JSONValue] = [
            "zone": .string(zone.id.address), "expected_revision": .string(revision)
        ]
        if let newTitle { body["title"] = .string(newTitle) }
        if operation != .save {
            guard let favorite else { throw ContractError.invalid("Choose an exact Sonos favorite.") }
            body["favorite"] = .string(favorite.id)
        }
        return body
    }
}

/// A read-only current-source or exact catalog observation plus the catalog
/// used to explain duplicate names. This is not a speaker-side lock.
struct FavoriteMutationObservation: Sendable {
    let catalog: [Favorite]
    let source: ObservedFavorite

    static func group(_ expected: Zone, in zones: [Zone]) throws -> Zone {
        guard let current = zones.first(where: { Announcement.sameGroup($0, expected) }) else {
            throw ContractError.invalid("The original group changed or is unavailable. Choose its current music or favorites and review again.")
        }
        return current
    }

    static func read(_ command: FavoriteMutation, client: DaemonClient) async throws -> Self {
        try Task.checkCancellation()
        _ = try group(command.zone, in: await client.zones())
        let catalog = try await client.managedFavorites(command.zone)
        let source: ObservedFavorite
        if command.operation == .save {
            source = try await client.currentFavorite(command.zone)
        } else {
            guard let expected = command.favorite,
                  catalog.filter({ $0.id.utf8.elementsEqual(expected.id.utf8) }).count == 1,
                  catalog.contains(where: { $0.sameSelection(as: expected) }) else {
                throw ContractError.invalid("The selected favorite changed or disappeared. Refresh its original household and review again.")
            }
            source = try await client.inspectFavorite(expected, in: command.zone)
        }
        _ = try group(command.zone, in: await client.zones())
        try Task.checkCancellation()
        return Self(catalog: catalog, source: source)
    }

    func validate(_ current: Self, command: FavoriteMutation) throws {
        guard source.zone.utf8.elementsEqual(current.source.zone.utf8),
              source.favorite.sameSelection(as: current.source.favorite),
              source.revision.utf8.elementsEqual(current.source.revision.utf8) else {
            throw ContractError.invalid(command.operation == .save
                ? "The current track or station changed after review. Review the actual source again; nothing was sent."
                : "The favorite changed after review. Read its current catalog entry and review again; nothing was sent.")
        }
        if command.operation == .save {
            let titled: ([Favorite]) -> Set<String> = { values in
                Set(values.filter { $0.title.utf8.elementsEqual((command.newTitle ?? "").utf8) }.map(\.id))
            }
            guard titled(catalog) == titled(current.catalog) else {
                throw ContractError.invalid("The household's matching favorite titles changed. Review the save again; equal titles can identify separate favorites.")
            }
        }
    }
}

public struct FavoriteMutationReview: Identifiable, Sendable {
    public let id = UUID()
    public let command: FavoriteMutation
    public let observedAt = Date()
    let observation: FavoriteMutationObservation
    public var favorite: Favorite { observation.source.favorite }
    public var currentSource: String { favorite.title }
    public var sourceKind: String { favorite.kind }
    public var sourceDescription: String? { favorite.description }
    public var nativeID: String? { favorite.id.isEmpty ? nil : favorite.id }
    public var duplicateTitles: Int {
        observation.catalog.filter { $0.title.utf8.elementsEqual((command.newTitle ?? "").utf8) }.count
    }
    var body: [String: JSONValue] { get throws { try command.fields(revision: observation.source.revision) } }

    func receipt(_ data: Data) throws -> FavoriteMutationReceipt {
        struct Reply: Decodable {
            let done: String
            let zone: String
            let favorite: Favorite
            let operation: String
            let changed: Bool
            let logged: Bool
        }
        let value = try DaemonClient.decode(Reply.self, from: data)
        _ = try value.favorite.validated()
        let expectedTitle = command.operation == .remove ? favorite.title : command.newTitle!
        let expectedChange = command.operation != .rename || !favorite.title.utf8.elementsEqual(expectedTitle.utf8)
        // save_current drops only blank descriptions before retaining the
        // observed source; rename/removal return the existing description.
        let expectedDescription = command.operation == .save
            ? favorite.description.flatMap({ FavoriteMutation.trimmed($0).isEmpty ? nil : $0 })
            : favorite.description
        guard !FavoriteMutation.trimmed(value.done).isEmpty, value.done.utf8.count <= 16_384,
              value.zone.utf8.elementsEqual(command.zone.coordinatorRoom.utf8),
              value.operation == command.operation.rawValue, value.changed == expectedChange,
              value.favorite.title.utf8.elementsEqual(expectedTitle.utf8),
              value.favorite.kind.utf8.elementsEqual(favorite.kind.utf8),
              value.favorite.description.map({ Array($0.utf8) }) == expectedDescription.map({ Array($0.utf8) }) else {
            throw ClientError.unreadable
        }
        if command.operation == .save {
            guard !observation.catalog.contains(where: { $0.id.utf8.elementsEqual(value.favorite.id.utf8) }) else {
                throw ClientError.unreadable
            }
        } else {
            guard value.favorite.id.utf8.elementsEqual(favorite.id.utf8) else {
                throw ClientError.unreadable
            }
        }
        return FavoriteMutationReceipt(done: value.done, favorite: value.favorite,
                                       changed: value.changed, logged: value.logged)
    }
}

public struct FavoriteMutationReceipt: Equatable, Sendable {
    public let done: String
    public let favorite: Favorite
    public let changed: Bool
    public let logged: Bool
    public var detail: String {
        "\(done)\nSonos favorite: \(favorite.id) · \(favorite.title)" + (logged ? "" :
            "\nThe favorite result was confirmed, but the action log could not be written. Do not repeat the operation.")
    }
}

public struct FavoriteManagementResult: Identifiable, Sendable {
    public enum Kind: Sendable { case confirmed, uncertain, notSent }
    public let id = UUID()
    public let command: FavoriteMutation
    public let receipt: FavoriteMutationReceipt?
    public let kind: Kind
    public let detail: String
    public var title: String {
        switch kind {
        case .confirmed: receipt?.done ?? "Sonos favorite updated"
        case .uncertain: "Favorite outcome uncertain — not repeated"
        case .notSent: "Favorite change was not sent"
        }
    }
}

/// An explicit original-household read never acknowledges a lost write.
public struct FavoriteCatalogRecovery: Sendable {
    public let target: RoomID
    public let catalog: [Favorite]
    public let observedAt: Date
    public let detail: String

    static func read(_ result: FavoriteManagementResult, client: DaemonClient) async throws -> Self {
        let target = result.command.zone.id
        let zones = try await client.zones()
        guard let current = zones.first(where: { zone in
            zone.household.utf8.elementsEqual(target.household.utf8) &&
                zone.members.contains(where: { $0.utf8.elementsEqual(target.room.utf8) })
        }) else {
            throw ContractError.invalid("The original room \(target.address) is unavailable. This favorite result cannot be checked through another household.")
        }
        let catalog = try await client.managedFavorites(current)
        _ = try FavoriteMutationObservation.group(current, in: await client.zones())
        try Task.checkCancellation()
        let note: String
        if result.command.operation == .save {
            let count = catalog.filter { $0.title.utf8.elementsEqual((result.command.newTitle ?? "").utf8) }.count
            note = "This household now has \(count) favorite(s) with the requested title. Equal titles can identify different sources; a matching title does not prove that the earlier save succeeded."
        } else if let original = result.command.favorite,
                  let found = catalog.first(where: { $0.id.utf8.elementsEqual(original.id.utf8) }) {
            note = "\(original.id) is currently listed as “\(found.title)”. This is its present catalog state, not confirmation of an earlier lost reply."
        } else {
            note = "The selected native favorite ID is not currently listed. Its absence does not prove which request removed it."
        }
        return Self(target: target, catalog: catalog, observedAt: Date(), detail: note + " Nothing was repeated.")
    }
}
