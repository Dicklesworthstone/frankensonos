import Foundation

/// A bounded destination observation for source insertion and catalog queue
/// replacement. It is a comparison receipt, not a lock on a speaker queue.
struct SourceQueueReview: Decodable, Sendable {
    let zone: String
    let total: UInt32
    let revision: String
    let playing: UInt32?
    let items: [QueueItem]

    static func read(_ client: DaemonClient, in zone: Zone) async throws -> Self {
        let value = try await client.get(Self.self, ["zones", zone.id.address, "queue"], query: [
            URLQueryItem(name: "offset", value: "0"), URLQueryItem(name: "limit", value: "1")
        ])
        guard value.zone.utf8.elementsEqual(zone.coordinatorRoom.utf8),
              !value.revision.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              value.revision.utf8.count <= 256,
              !value.revision.unicodeScalars.contains(where: CharacterSet.controlCharacters.contains),
              value.items.count == (value.total == 0 ? 0 : 1),
              value.items.first.map({ $0.position == 1 }) ?? (value.total == 0),
              value.playing.map({ $0 > 0 && $0 <= value.total }) ?? true else {
            throw ContractError.invalid("The destination queue could not be read consistently. Refresh it before reviewing this source.")
        }
        return value
    }

    func validate(_ current: Self, command: NativeCommand) throws {
        guard zone.utf8.elementsEqual(current.zone.utf8), total == current.total,
              revision.utf8.elementsEqual(current.revision.utf8),
              PlaylistContentsRequest.exact(items.first?.uri, current.items.first?.uri),
              !command.sourceQueueNeedsOccurrence || playing == current.playing else {
            throw ContractError.invalid("The destination queue changed after review. Review this source again; nothing was sent.")
        }
    }
}

extension NativeCommand {
    var needsSourceQueueReview: Bool {
        switch self {
        case .queueAdd, .queueFavorite: true
        case .playlistTrack(let selection): selection.isQueueAdd
        case .playSource(let uri):
            if let canonical = try? SceneDefinition.sourceURI(uri) {
                SpotifyCatalogKind.artist.contains(canonical) || SpotifyCatalogKind.show.contains(canonical)
            } else { false }
        default: false
        }
    }

    var sourceQueueNeedsOccurrence: Bool {
        switch self {
        case .queueAdd(_, let next): next
        case .queueFavorite(let favorite): favorite.next
        case .playlistTrack(let selection): !selection.allowsTrackProgress
        case .playSource: true
        default: false
        }
    }
}
