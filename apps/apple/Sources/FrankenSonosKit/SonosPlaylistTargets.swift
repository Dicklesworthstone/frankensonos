import Foundation

/// What the listener reviewed for one destination. The queue is an observation;
/// only a single-target request can also carry its queue revision to the daemon.
public struct SonosPlaylistDestinationReview: Identifiable, Sendable {
    public let zone: Zone
    public let state: ZoneState
    public let queueTotal: UInt32
    public let playing: UInt32?
    public var id: RoomID { zone.id }
}

/// The daemon resolves aliases and complete physical groups. Room names alone
/// cannot establish this audience: the revision also covers player identities.
struct SonosPlaylistAudience: Decodable, Equatable, Sendable {
    struct Room: Decodable, Equatable, Sendable {
        let room: String
        let household: String
        let player: String
        let coordinator: String
    }
    struct Group: Decodable, Equatable, Sendable {
        let household: String
        let coordinator: String
        let coordinatorRoom: String
        let members: [Room]
        var id: String { household + "\u{0}" + coordinator }
    }
    let target: String
    let revision: String
    let matched: String
    let rooms: [Room]
    let groups: [Group]

    static func validText(_ value: String) -> Bool {
        !value.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            && value.utf8.count <= 4_096
            && !value.unicodeScalars.contains(where: CharacterSet.controlCharacters.contains)
    }

    func zones(for source: Zone, target expected: String, in available: [Zone]) throws -> [Zone] {
        guard PlaylistContentsRequest.exact(target, expected),
              revision.utf8.count == 64,
              revision.utf8.allSatisfy({ (48...57).contains($0) || (97...102).contains($0) }),
              ["room", "alias", "here", "everywhere", "prefix"].contains(matched),
              (1...256).contains(groups.count), (1...256).contains(rooms.count),
              groups.reduce(0, { $0 + $1.members.count }) <= 256 else {
            throw ContractError.invalid("The destination audience could not be read completely. Choose it again.")
        }
        var groupIDs = Set<String>(), roomIDs = Set<RoomID>(), players = Set<String>()
        var result: [Zone] = []
        for group in groups {
            guard group.household == source.household,
                  Self.validText(group.coordinator), groupIDs.insert(group.id).inserted,
                  !group.members.isEmpty,
                  let zone = available.first(where: {
                      $0.household == group.household && $0.coordinatorRoom == group.coordinatorRoom
                  }),
                  Set(group.members.map(\.room)) == Set(zone.members),
                  group.members.count == zone.members.count else {
                throw ContractError.invalid("Choose destination groups within the saved playlist's original household.")
            }
            for room in group.members {
                let id = RoomID(household: room.household, room: room.room)
                guard room.household == group.household, room.coordinator == group.coordinator,
                      Self.validText(room.room), Self.validText(room.player),
                      roomIDs.insert(id).inserted, players.insert(room.player).inserted else {
                    throw ContractError.invalid("The destination group identities are inconsistent. Review them again.")
                }
            }
            result.append(try zone.validated())
        }
        var selected = Set<RoomID>(), covered = Set<String>()
        for room in rooms {
            let id = RoomID(household: room.household, room: room.room)
            guard selected.insert(id).inserted,
                  let group = groups.first(where: { $0.members.contains(room) }) else {
                throw ContractError.invalid("The selected rooms do not match the complete destination groups.")
            }
            covered.insert(group.id)
        }
        guard covered == groupIDs else {
            throw ContractError.invalid("A destination group is missing from the selected audience.")
        }
        return result
    }

    func matches(_ other: Self) -> Bool {
        PlaylistContentsRequest.exact(target, other.target)
            && PlaylistContentsRequest.exact(revision, other.revision)
            && self == other
    }

    var summary: String {
        groups.map { "\($0.members.map(\.room).joined(separator: " + ")) · \($0.household)" }
            .joined(separator: "\n")
    }
}

struct SonosPlaylistTargetObservation: Sendable {
    struct Destination: Sendable {
        let zone: Zone
        let state: ZoneState
        let queue: SourceQueueReview
        var review: SonosPlaylistDestinationReview {
            SonosPlaylistDestinationReview(zone: zone, state: state, queueTotal: queue.total, playing: queue.playing)
        }
    }
    let audience: SonosPlaylistAudience
    let destinations: [Destination]

    static func read(_ client: DaemonClient, selection: SonosPlaylistTracksSelection,
                     zones: [Zone]) async throws -> Self {
        guard let target = selection.target else {
            throw ContractError.invalid("Choose the destination before reviewing selected tracks.")
        }
        try Task.checkCancellation()
        let audience = try await client.get(SonosPlaylistAudience.self, ["rooms", "resolve"], query: [
            URLQueryItem(name: "target", value: target)
        ])
        try Task.checkCancellation()
        let targets = try audience.zones(for: selection.zone, target: target, in: zones)
        if let expected = selection.audience, !expected.matches(audience) {
            throw ContractError.invalid("The reviewed destination audience changed. Choose its current groups again.")
        }
        var destinations: [Destination] = []
        for zone in targets {
            try Task.checkCancellation()
            let state = try await client.state(zone)
            try Task.checkCancellation()
            let queue = try await SourceQueueReview.read(client, in: zone)
            try Task.checkCancellation()
            destinations.append(Destination(zone: zone, state: state, queue: queue))
        }
        return Self(audience: audience, destinations: destinations)
    }

    func validate(_ current: Self, selection: SonosPlaylistTracksSelection) throws {
        guard audience.matches(current.audience), destinations.count == current.destinations.count else {
            throw ContractError.invalid("The reviewed destination audience changed. Nothing was sent.")
        }
        for before in destinations {
            guard let after = current.destinations.first(where: { before.zone.sameGroup(as: $0.zone) }) else {
                throw ContractError.invalid("A destination group changed. Review all destinations again.")
            }
            try before.queue.validate(after.queue, command: .sonosPlaylistTracks(selection))
            if selection.action != .append, !before.state.samePlayback(as: after.state) {
                throw ContractError.invalid("Playback changed in \(before.zone.id.address). Review the selected tracks again.")
            }
        }
    }

    /// Bracket slow native-source reads with destination observations. These
    /// checks are not reservations; the daemon still prepares current queues.
    func confirm(_ client: DaemonClient, selection: SonosPlaylistTracksSelection) async throws {
        let zones = try await client.zones()
        try Task.checkCancellation()
        let current = try await Self.read(client, selection: selection, zones: zones)
        try validate(current, selection: selection)
    }
}
