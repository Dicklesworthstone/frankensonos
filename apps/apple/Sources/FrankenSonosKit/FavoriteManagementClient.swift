import Foundation

extension Favorite {
    /// Current-source previews have no native identity until Sonos saves them.
    /// Catalog edits always retain an exact household-local FV:2/<n> identity.
    public func validated(current: Bool = false) throws -> Self {
        let nativeID = id.hasPrefix("FV:2/") && id.utf8.count > 5 && id.utf8.count <= 1_024
            && id.dropFirst(5).utf8.allSatisfy({ (48...57).contains($0) })
        guard current ? id.isEmpty : nativeID,
              (current ? ["track", "stream"] : ["track", "stream", "container", "unplayable"]).contains(kind),
              !FavoriteMutation.trimmed(title).isEmpty, title.utf8.count <= 16_384,
              !title.unicodeScalars.contains(where: { $0.properties.generalCategory == .control }),
              description.map({ $0.utf8.count <= 16_384 }) ?? true else {
            throw ClientError.unreadable
        }
        return self
    }

    func sameSelection(as other: Self) -> Bool {
        id.utf8.elementsEqual(other.id.utf8) && title.utf8.elementsEqual(other.title.utf8)
            && kind.utf8.elementsEqual(other.kind.utf8)
            && description.map({ Array($0.utf8) }) == other.description.map({ Array($0.utf8) })
    }
}

/// Only display metadata and an opaque source/catalog observation cross the
/// API boundary. Renderer resources and DIDL remain on the daemon.
public struct ObservedFavorite: Decodable, Equatable, Sendable {
    public let zone: String
    public let favorite: Favorite
    public let revision: String

    func validated(in expected: Zone, current: Bool) throws -> Self {
        _ = try favorite.validated(current: current)
        guard zone.utf8.elementsEqual(expected.coordinatorRoom.utf8),
              FavoriteMutation.validRevision(revision) else { throw ClientError.unreadable }
        return self
    }
}

extension DaemonClient {
    public func managedFavorites(_ zone: Zone) async throws -> [Favorite] {
        _ = try zone.validated()
        try Task.checkCancellation()
        let catalog = try await favorites(zone)
        try Task.checkCancellation()
        guard catalog.count <= 10_000 else { throw ClientError.unreadable }
        var ids = Set<String>()
        for favorite in catalog {
            _ = try favorite.validated()
            guard ids.insert(favorite.id).inserted else { throw ClientError.unreadable }
        }
        return catalog
    }

    public func currentFavorite(_ zone: Zone) async throws -> ObservedFavorite {
        _ = try zone.validated()
        try Task.checkCancellation()
        let observed = try await get(ObservedFavorite.self, ["favorites", "current"],
            query: [URLQueryItem(name: "zone", value: zone.id.address)], timeout: 120)
        try Task.checkCancellation()
        return try observed.validated(in: zone, current: true)
    }

    public func inspectFavorite(_ favorite: Favorite, in zone: Zone) async throws -> ObservedFavorite {
        _ = try zone.validated()
        _ = try favorite.validated()
        try Task.checkCancellation()
        let observed = try await get(ObservedFavorite.self, ["favorites", "inspect"], query: [
            URLQueryItem(name: "zone", value: zone.id.address),
            URLQueryItem(name: "favorite", value: favorite.id)
        ], timeout: 120).validated(in: zone, current: false)
        try Task.checkCancellation()
        guard observed.favorite.sameSelection(as: favorite) else {
            throw ContractError.invalid("The selected favorite changed. Refresh its original household and choose the exact entry again.")
        }
        return observed
    }
}
