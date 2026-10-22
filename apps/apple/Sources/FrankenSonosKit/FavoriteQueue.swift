import Foundation

/// A native favorite belongs to one observed household/group. Its renderer
/// metadata stays on the daemon; never turn it into an arbitrary source URI.
public struct FavoriteQueue: Equatable, Sendable {
    public let favorite: Favorite
    public let zone: Zone
    public let next: Bool

    public init(_ favorite: Favorite, in zone: Zone, next: Bool) throws {
        _ = try zone.validated()
        guard Self.canQueue(favorite) else {
            throw ContractError.invalid("Only a saved track, album or playlist can be queued. Play stations directly; refresh unreadable favorites.")
        }
        self.favorite = favorite; self.zone = zone; self.next = next
    }

    public static func canQueue(_ favorite: Favorite) -> Bool {
        ["track", "container"].contains(favorite.kind) &&
        !favorite.id.isEmpty && favorite.id.unicodeScalars.count <= 1_024 &&
        favorite.id == favorite.id.trimmingCharacters(in: .whitespacesAndNewlines) &&
        !favorite.id.unicodeScalars.contains(where: CharacterSet.controlCharacters.contains) &&
        !favorite.title.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }

    public var title: String { next ? "Queue favorite next" : "Add favorite to queue" }
    public var explanation: String {
        "Adds “\(favorite.title)” from \(zone.household) without replacing the queue or starting playback. " +
        (next ? "Inserts after the current queue track, or appends when the group is not playing a queue track. "
              : "Appends to the end of this group’s queue. ") +
        "The daemon keeps the favorite’s native metadata and expands an album or playlist on the speaker."
    }

    func fields(in current: Zone) throws -> [String: JSONValue] {
        _ = try current.validated()
        guard zone.sameGroup(as: current) else {
            throw ContractError.invalid("The favorite’s destination group changed. Refresh favorites in the intended household.")
        }
        return ["zone": .string(zone.id.address), "favorite": .string(favorite.id), "next": .bool(next)]
    }

    /// No title/position fallback: IDs and the visible definition must still
    /// match. This is a preflight, not a lock on the household's favorite store.
    func validate(_ current: [Favorite], in zone: Zone) throws -> Favorite {
        _ = try fields(in: zone)
        let matches = current.filter { $0.id.utf8.elementsEqual(favorite.id.utf8) }
        guard matches.count == 1, let found = matches.first, Self.canQueue(found),
              found.title.utf8.elementsEqual(favorite.title.utf8), found.kind == favorite.kind,
              found.description.map({ Array($0.utf8) }) == favorite.description.map({ Array($0.utf8) }) else {
            throw ContractError.invalid("The selected favorite changed, disappeared or is ambiguous. Refresh favorites and review it again.")
        }
        return found
    }

    func receipt(_ data: Data, in current: Zone?) throws -> CommandReceipt {
        if let current { _ = try fields(in: current) }
        struct Added: Decodable {
            let done: String
            let zone: String
            let first: UInt32
            let count: UInt32
            let playingQueue: Bool
        }
        let report = try DaemonClient.decode(Added.self, from: data)
        guard !report.done.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              report.zone.utf8.elementsEqual(zone.coordinatorRoom.utf8), report.first > 0, report.count > 0,
              UInt64(report.first) + UInt64(report.count) - 1 <= UInt64(UInt32.max) else {
            throw ClientError.unreadable
        }
        let playback = report.playingQueue ? "Playback was not started or restarted."
            : "The group is not playing its queue; its current source was left unchanged."
        return CommandReceipt(title: report.done,
            detail: "Added \(report.count) entries from “\(favorite.title)” at position \(report.first).\n" + playback)
    }
}
