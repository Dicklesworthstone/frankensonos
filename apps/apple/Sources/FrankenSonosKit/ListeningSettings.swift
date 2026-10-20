import Foundation

/// Tone belongs to the named speaker, not necessarily its group's coordinator.
public struct SpeakerTone: Decodable, Equatable, Sendable {
    public let room: String
    public let bass: Int
    public let treble: Int
    public let loudness: Bool

    public func validated(room expected: String) throws -> Self {
        guard !expected.isEmpty, room == expected,
              (-10...10).contains(bass), (-10...10).contains(treble) else {
            throw ContractError.invalid("The speaker sound is incomplete or belongs to another room. Read it again.")
        }
        return self
    }
    public var summary: String {
        "Bass \(Self.level(bass)), treble \(Self.level(treble)), loudness \(loudness ? "on" : "off")"
    }
    public static func level(_ value: Int) -> String { value > 0 ? "+\(value)" : String(value) }
}

public struct SpeakerToneChange: Equatable, Sendable {
    public let target: RoomID
    public let before: SpeakerTone
    public let desired: SpeakerTone

    public init(target: RoomID, before: SpeakerTone, bass: Int, treble: Int, loudness: Bool) throws {
        self.target = target
        self.before = try before.validated(room: target.room)
        desired = try SpeakerTone(room: target.room, bass: bass, treble: treble, loudness: loudness)
            .validated(room: target.room)
        guard !target.household.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              desired != before else { throw ContractError.invalid("Choose a sound change before reviewing it.") }
    }

    public func validate(_ current: SpeakerTone) throws {
        guard try current.validated(room: target.room) == before else {
            throw ContractError.invalid("This speaker's sound changed. Read its current tone and review the change again.")
        }
    }
    /// Only edited controls are sent. No volume, mute or group flag is invented.
    public func fields(in group: Zone) throws -> [String: JSONValue] {
        _ = try group.validated()
        guard group.rooms.contains(target) else {
            throw ContractError.invalid("The reviewed speaker no longer belongs to this household and group.")
        }
        var fields: [String: JSONValue] = ["zone": .string(target.address)]
        if before.bass != desired.bass { fields["bass"] = .integer(Int64(desired.bass)) }
        if before.treble != desired.treble { fields["treble"] = .integer(Int64(desired.treble)) }
        if before.loudness != desired.loudness { fields["loudness"] = .bool(desired.loudness) }
        return fields
    }
}

/// SHUFFLE means shuffle AND repeat-all; do not treat it as a boolean toggle.
public enum QueuePlaybackMode: String, Codable, CaseIterable, Sendable {
    case normal = "NORMAL", repeatAll = "REPEAT_ALL", repeatOne = "REPEAT_ONE"
    case shuffle = "SHUFFLE", shuffleNoRepeat = "SHUFFLE_NOREPEAT", shuffleRepeatOne = "SHUFFLE_REPEAT_ONE"

    public enum Repetition: String, CaseIterable, Sendable {
        case off, queue, track
        public var title: String {
            switch self { case .off: "Off"; case .queue: "Whole queue"; case .track: "Current track" }
        }
    }
    public var shuffled: Bool { [.shuffle, .shuffleNoRepeat, .shuffleRepeatOne].contains(self) }
    public var repetition: Repetition {
        switch self {
        case .normal, .shuffleNoRepeat: .off
        case .repeatAll, .shuffle: .queue
        case .repeatOne, .shuffleRepeatOne: .track
        }
    }
    public init(shuffled: Bool, repetition: Repetition) {
        switch (shuffled, repetition) {
        case (false, .off): self = .normal
        case (false, .queue): self = .repeatAll
        case (false, .track): self = .repeatOne
        case (true, .off): self = .shuffleNoRepeat
        case (true, .queue): self = .shuffle
        case (true, .track): self = .shuffleRepeatOne
        }
    }
    public var summary: String { "\(shuffled ? "Shuffle" : "In order") · Repeat: \(repetition.title.lowercased())" }
}

public struct QueuePlaybackOrder: Decodable, Equatable, Sendable {
    public let zone: String
    public let mode: QueuePlaybackMode
    public let queueActive: Bool
    public let djActive: Bool

    public func validated(in group: Zone) throws -> Self {
        _ = try group.validated()
        guard zone == group.coordinatorRoom else {
            throw ContractError.invalid("The playback-order observation belongs to another group. Refresh it.")
        }
        return self
    }
    public func allows(_ mode: QueuePlaybackMode) -> Bool { queueActive && (!djActive || mode == .normal) }
    public var limitation: String? {
        if !queueActive { return "Another source is selected. These controls will not replace it or start the queue." }
        if djActive { return "Your DJ keeps whole works in sequence. Stop the DJ before choosing shuffle or repeat." }
        return nil
    }
}

public struct PlaybackOrderChange: Equatable, Sendable {
    public let group: Zone
    public let before: QueuePlaybackOrder
    public let desired: QueuePlaybackMode

    public init(group: Zone, before: QueuePlaybackOrder, desired: QueuePlaybackMode) throws {
        self.group = try group.validated()
        self.before = try before.validated(in: group)
        self.desired = desired
        guard before.allows(desired), before.mode != desired else {
            throw ContractError.invalid(before.limitation ?? "Choose a different playback order before reviewing it.")
        }
    }
    public func validate(_ current: QueuePlaybackOrder) throws {
        guard try current.validated(in: group) == before, current.allows(desired) else {
            throw ContractError.invalid("The source, DJ or playback order changed. Refresh and review the setting again.")
        }
    }
    public func fields(in current: Zone) throws -> [String: JSONValue] {
        _ = try current.validated()
        guard group.sameGroup(as: current) else {
            throw ContractError.invalid("The reviewed group changed. Choose its current playback order again.")
        }
        return ["zone": .string(group.id.address), "mode": .string(desired.rawValue),
                "expected_mode": .string(before.mode.rawValue)]
    }
}

/// These are the backend's actual acknowledgements, not generic Outcome DTOs.
public struct SpeakerToneReply: Decodable, Sendable {
    public let done: String
    public let eq: SpeakerTone
    public func validated(for change: SpeakerToneChange) throws -> Self {
        guard !done.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              try eq.validated(room: change.target.room) == change.desired else {
            throw ContractError.invalid("The speaker did not confirm the requested tone. The change may be partial; read its sound again.")
        }
        return self
    }
}
public struct PlaybackOrderReply: Decodable, Sendable {
    public let done: String
    public let state: QueuePlaybackOrder
    public let changed: Bool
    public let recovered: Bool
    public func validated(for change: PlaybackOrderChange) throws -> Self {
        _ = try state.validated(in: change.group)
        guard !done.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              state.mode == change.desired, state.allows(change.desired) else {
            throw ContractError.invalid("The requested playback order was not confirmed. Read the current setting again.")
        }
        return self
    }
    public var detail: String {
        recovered ? "\(done)\nThe daemon observed the requested mode after a lost speaker reply. This does not prove which controller changed it." : done
    }
}

public enum ListeningSettingsScope: Equatable, Sendable {
    case speaker(Zone, room: String)
    case order(Zone)
    public var group: Zone { switch self { case .speaker(let group, _), .order(let group): group } }
    public var target: RoomID {
        switch self {
        case .speaker(let group, let room): RoomID(household: group.household, room: room)
        case .order(let group): group.id
        }
    }
    public static func == (left: Self, right: Self) -> Bool {
        switch (left, right) {
        case (.speaker(let a, let ar), .speaker(let b, let br)): a.sameGroup(as: b) && ar == br
        case (.order(let a), .order(let b)): a.sameGroup(as: b)
        default: false
        }
    }
    public func validated() throws -> Self {
        _ = try group.validated()
        guard group.rooms.contains(target) else { throw ContractError.invalid("Choose a speaker in this group.") }
        return self
    }
}
public enum ListeningSettingsValue: Equatable, Sendable {
    case speaker(SpeakerTone), order(QueuePlaybackOrder)
    public func validated(for scope: ListeningSettingsScope) throws -> Self {
        _ = try scope.validated()
        switch (self, scope) {
        case (.speaker(let value), .speaker(_, let room)): _ = try value.validated(room: room)
        case (.order(let value), .order(let group)): _ = try value.validated(in: group)
        default: throw ContractError.invalid("The response does not match these listening controls.")
        }
        return self
    }
}
public enum ListeningSettingsChange: Equatable, Sendable {
    case speaker(SpeakerToneChange), order(PlaybackOrderChange)
}
