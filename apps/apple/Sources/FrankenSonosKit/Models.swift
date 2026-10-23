import Foundation

public enum ContractError: Error, LocalizedError, Sendable, Equatable {
    case invalid(String)
    public var errorDescription: String? {
        switch self { case .invalid(let message): message }
    }
}

/// Room names alone are not identities: S1 and S2 may contain the same name.
public struct RoomID: Hashable, Codable, Sendable, Identifiable {
    public let household: String
    public let room: String
    public var id: Self { self }
    public var address: String { "\(room)@\(household)" }
    public init(household: String, room: String) { self.household = household; self.room = room }
}

public struct Zone: Codable, Hashable, Sendable, Identifiable {
    public let household: String
    public let coordinatorRoom: String
    public let members: [String]
    public let transportState: String
    public var id: RoomID { RoomID(household: household, room: coordinatorRoom) }
    public var rooms: Set<RoomID> { Set(members.map { RoomID(household: household, room: $0) }) }
    public var title: String { members.joined(separator: " + ") }
    public func sameGroup(as other: Zone) -> Bool { id == other.id && rooms == other.rooms }
    public func validated() throws -> Self {
        guard !household.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              !coordinatorRoom.isEmpty, !members.isEmpty, members.count <= 256,
              members.allSatisfy({ !$0.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty }),
              members.contains(coordinatorRoom), Set(members).count == members.count else {
            throw ContractError.invalid("The room grouping is incomplete. Refresh the house.")
        }
        return self
    }
    public static func validate(_ zones: [Zone]) throws -> [Zone] {
        guard zones.count <= 256 else { throw ContractError.invalid("Too many groups in the response.") }
        var seen = Set<RoomID>()
        for zone in zones {
            _ = try zone.validated()
            guard seen.isDisjoint(with: zone.rooms) else {
                throw ContractError.invalid("A room belongs to more than one group.")
            }
            seen.formUnion(zone.rooms)
        }
        return zones
    }
}

public struct Track: Codable, Equatable, Sendable {
    public let uri: String
    public let title: String?
    public let creator: String?
    public let album: String?
    public let durationSecs: UInt32?
    public let positionSecs: UInt32?
    public let queuePosition: UInt32?
    public var displayTitle: String { title.flatMap { $0.isEmpty ? nil : $0 } ?? "Untitled audio" }
    public func sameItem(as other: Track?) -> Bool {
        guard let other else { return false }
        return uri == other.uri && queuePosition == other.queuePosition && durationSecs == other.durationSecs
    }
}

public struct ZoneState: Codable, Equatable, Sendable {
    public let zone: Zone
    public let transportState: String
    public let volume: Int?
    public let mute: Bool?
    public let groupVolume: Int?
    public let track: Track?
    public let shuffle: Bool?
    public let `repeat`: String?
    public var isPlaying: Bool { transportState == "playing" }
    public func validated(for expected: Zone) throws -> Self {
        _ = try zone.validated()
        guard zone.sameGroup(as: expected), ["playing", "paused", "stopped", "transitioning", "unknown"].contains(transportState),
              [volume, groupVolume].allSatisfy({ $0 == nil || (0...100).contains($0!) }),
              track?.queuePosition != 0,
              track == nil || !(track!.uri.isEmpty || track!.uri == "NOT_IMPLEMENTED") else {
            throw ContractError.invalid("Playback no longer matches the selected group. Refresh the house.")
        }
        if let duration = track?.durationSecs, duration > 0, let position = track?.positionSecs, position > duration {
            throw ContractError.invalid("The playback position is outside the track.")
        }
        return self
    }
    public func samePlayback(as other: ZoneState) -> Bool {
        zone.sameGroup(as: other.zone) && transportState == other.transportState &&
        (track == nil ? other.track == nil : track!.sameItem(as: other.track))
    }
}

public struct QueueItem: Codable, Equatable, Sendable, Identifiable {
    public let position: UInt32
    public let title: String
    public let uri: String?
    public let creator: String?
    public let album: String?
    public var id: UInt32 { position }
    public var playable: Bool { uri.map { !$0.isEmpty && $0 != "NOT_IMPLEMENTED" } ?? false }
}
public struct QueuePage: Codable, Equatable, Sendable {
    public let total: UInt32
    public let items: [QueueItem]
    public let playing: UInt32?
    public let revision: String?
    public func validated(offset: UInt32, limit: UInt32 = 25) throws -> Self {
        guard items.count <= limit, UInt64(offset) + UInt64(items.count) <= UInt64(max(total, offset)),
              !items.isEmpty || offset >= total,
              items.enumerated().allSatisfy({ UInt64($0.element.position) == UInt64(offset) + UInt64($0.offset) + 1 }) else {
            throw ContractError.invalid("The queue changed while it was read. Refresh the queue.")
        }
        return self
    }
}

public struct Favorite: Codable, Equatable, Sendable, Identifiable {
    public let id: String
    public let title: String
    public let kind: String
    public let description: String?
    public var playable: Bool { ["track", "stream", "container"].contains(kind) }
}
public struct LibraryHit: Codable, Equatable, Sendable, Identifiable {
    public let title: String
    public let kind: String
    public let subtitle: String?
    public let sourceUri: String?
    public let favorite: String?
    public var id: String { "\(kind):\(sourceUri ?? favorite ?? title)" }
}
public struct Note: Codable, Equatable, Sendable {
    public let code: String
    public let detail: String
}
public struct Outcome: Codable, Equatable, Sendable {
    public let done: String
    public let changed: Bool
    public let volume: Int?
    public let notes: [Note]?
    public var detail: String { ([done] + (notes ?? []).map(\.detail)).joined(separator: "\n") }
}
public struct QueueAdded: Codable, Equatable, Sendable {
    public let done: String
    public let first: UInt32
    public let count: UInt32
    public let playingQueue: Bool
    public let notes: [Note]?
}

public struct DJWork: Codable, Equatable, Sendable {
    public let title: String
    public let composer: String
    public let performers: [String]?
    public let movements: UInt32
    public let movement: UInt32?
    public let movementTitle: String?
    public let minutes: UInt32?
    public let reason: Reason
    public struct Reason: Codable, Equatable, Sendable {
        public let summary: String
        public let factors: [Factor]?
        public let relaxed: [String]?
    }
    public struct Factor: Codable, Equatable, Sendable {
        public let factor: String
        public let weight: Int
    }
}
public struct DJStatus: Codable, Equatable, Sendable {
    public let zone: String
    public let running: Bool
    public let now: DJWork?
    public let next: [DJWork]
    public let steering: Steering
    public let problem: String?
    public struct Steering: Codable, Equatable, Sendable {
        public let source: String
        public let mood: String?
        public let expiresAt: Int64?
        public let expiresInSecs: UInt64?
        public let constraints: [String: JSONValue]?
        public let added: [String: JSONValue]?
    }
    public func validated(for expected: Zone) throws -> Self {
        guard zone == expected.coordinatorRoom, next.count <= 128,
              ([now].compactMap { $0 } + next).allSatisfy({ work in
                  work.movements > 0 && (work.movement == nil || (1...work.movements).contains(work.movement!))
              }) else { throw ContractError.invalid("The DJ observation belongs to a different group or is incomplete.") }
        return self
    }
    /// Countdown and cosmetic timing do not invalidate an otherwise identical review.
    public func sameSession(as other: Self) -> Bool {
        zone == other.zone && running == other.running && now == other.now && next == other.next &&
        steering.source == other.steering.source && steering.mood == other.steering.mood &&
        steering.expiresAt == other.steering.expiresAt && steering.added == other.steering.added &&
        steering.constraints == other.steering.constraints
    }
}
public struct MoodCatalog: Codable, Sendable {
    public let moods: [Mood]
    public struct Mood: Codable, Equatable, Sendable, Identifiable {
        public let name: String
        public let constraints: [String: JSONValue]
        public var id: String { name }
    }
}

public struct Scene: Codable, Equatable, Sendable, Identifiable {
    public let name: String
    public let saved: Int64
    public let groups: [Group]
    public let volumes: [String: Int]
    public let mutes: [String: Bool]
    // Keep distinct UTF-8 names distinct in SwiftUI, whose String equality
    // otherwise merges composed and decomposed Unicode spellings.
    public var id: String { Data(name.utf8).base64EncodedString() }
    public struct Group: Codable, Equatable, Sendable {
        public let coordinator: String
        public let members: [String]
        public let source: Source
        public let playing: Bool?
    }
    public struct Source: Codable, Equatable, Sendable {
        public let kind: String
        public let name: String?
        public let uri: String?
        public let mood: String?
        public let queue: QueueReference?
        public let direction: DJDirection?

        public init(kind: String, name: String?, uri: String?, mood: String?, queue: QueueReference? = nil,
                    direction: DJDirection? = nil) {
            self.kind = kind; self.name = name; self.uri = uri; self.mood = mood; self.queue = queue
            self.direction = direction
        }
    }
    /// A captured position on the original coordinator. A non-nil total
    /// identifies a durable backup whose queue contents remain on the daemon.
    public struct QueueReference: Codable, Equatable, Sendable {
        public let coordinator: String
        public let household: String?
        public let updateId: UInt32
        public let track: UInt32
        public let positionSecs: UInt32
        public let playMode: QueuePlaybackMode?
        public let bootSeq: UInt32?
        public let total: UInt32?

        public init(coordinator: String, household: String? = nil, updateId: UInt32, track: UInt32,
                    positionSecs: UInt32, playMode: QueuePlaybackMode? = nil, bootSeq: UInt32? = nil,
                    total: UInt32? = nil) {
            self.coordinator = coordinator; self.household = household; self.updateId = updateId
            self.track = track; self.positionSecs = positionSecs; self.playMode = playMode; self.bootSeq = bootSeq
            self.total = total
        }

        public var summary: String {
            let seconds = String(format: "%02d", positionSecs % 60)
            if let total {
                return "Saved queue backup · \(total) entries · track \(track) at \(positionSecs / 60):\(seconds)"
            }
            return "Original unchanged queue · track \(track) at \(positionSecs / 60):\(seconds)"
        }

        public var recallNote: String {
            if total != nil {
                return "Recall replaces the original player’s current queue. Action-history undo cannot restore the replaced queue contents."
            }
            return "Queue contents are not saved. Recall requires the original unchanged queue; a known speaker restart also invalidates this position."
        }
    }
}
public struct SceneReport: Codable, Equatable, Sendable {
    public let scene: String
    public let done: String
    public let changed: Bool
    public let complete: Bool
    public let steps: [String]
    public let failed: [StepFailure]?
    public let skipped: [String]?
    public let notes: [Note]?
    public struct StepFailure: Codable, Equatable, Sendable {
        public let step: String
        public let error: String
    }
}

/// Opaque constraint dictionaries are copied, not interpreted or reimplemented.
public enum JSONValue: Codable, Equatable, Sendable {
    case string(String), integer(Int64), number(Double), bool(Bool), array([Self]), object([String: Self]), null
    public init(from decoder: Decoder) throws {
        let c = try decoder.singleValueContainer()
        if c.decodeNil() { self = .null }
        else if let v = try? c.decode(Bool.self) { self = .bool(v) }
        else if let v = try? c.decode(Int64.self) { self = .integer(v) }
        else if let v = try? c.decode(Double.self) { self = .number(v) }
        else if let v = try? c.decode(String.self) { self = .string(v) }
        else if let v = try? c.decode([Self].self) { self = .array(v) }
        else { self = .object(try c.decode([String: Self].self)) }
    }
    public func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        switch self {
        case .string(let v): try c.encode(v)
        case .integer(let v): try c.encode(v)
        case .number(let v): try c.encode(v)
        case .bool(let v): try c.encode(v)
        case .array(let v): try c.encode(v)
        case .object(let v): try c.encode(v)
        case .null: try c.encodeNil()
        }
    }
}
