import Foundation

public enum ScheduleDay: String, CaseIterable, Sendable {
    case mon, tue, wed, thu, fri, sat, sun
}

/// Weekly times belong to the daemon's house, never this Mac's time zone.
/// One-shot times include their UTC offset. The daemon computes every next run,
/// DST adjustment and missed-run decision; the native client runs no scheduler.
public enum ScheduleTime: Equatable, Sendable {
    case weekly(days: Set<ScheduleDay>, at: String)
    case once(at: String)

    public func canonical() throws -> String {
        switch self {
        case .weekly(let days, let time):
            let bytes = Array(time.utf8)
            guard !days.isEmpty, bytes.count == 5, bytes[2] == 58,
                  [bytes[0], bytes[1], bytes[3], bytes[4]].allSatisfy({ (48...57).contains($0) }),
                  let hour = Int(time.prefix(2)), hour < 24,
                  let minute = Int(time.suffix(2)), minute < 60 else {
                throw ContractError.invalid("Choose at least one day and a house-local time in HH:MM form.")
            }
            let prefix: String
            if days.count == 7 { prefix = "daily" }
            else if days == [.mon, .tue, .wed, .thu, .fri] { prefix = "weekdays" }
            else if days == [.sat, .sun] { prefix = "weekends" }
            else { prefix = ScheduleDay.allCases.filter(days.contains).map(\.rawValue).joined(separator: ",") }
            return prefix + " " + time
        case .once(let at):
            guard Self.instant(at, fractional: false) != nil else {
                throw ContractError.invalid("Use a complete one-time date with seconds and a UTC offset, such as 2028-03-12T07:30:00-04:00.")
            }
            return "once " + at
        }
    }

    static func stored(_ input: String) throws -> Self {
        if input.hasPrefix("once ") {
            let time = Self.once(at: String(input.dropFirst(5)))
            _ = try time.canonical()
            return time
        }
        let parts = input.split(separator: " ", omittingEmptySubsequences: false)
        guard parts.count == 2 else { throw ContractError.invalid("The stored schedule time is unreadable.") }
        let days: Set<ScheduleDay>
        switch parts[0] {
        case "daily": days = Set(ScheduleDay.allCases)
        case "weekdays": days = [.mon, .tue, .wed, .thu, .fri]
        case "weekends": days = [.sat, .sun]
        default:
            let names = parts[0].split(separator: ",", omittingEmptySubsequences: false)
            let parsed = names.compactMap { ScheduleDay(rawValue: String($0)) }
            guard parsed.count == names.count else { throw ContractError.invalid("The stored schedule days are unreadable.") }
            days = Set(parsed)
        }
        let time = Self.weekly(days: days, at: String(parts[1]))
        _ = try time.canonical()
        return time
    }

    func sameTime(as other: Self) -> Bool {
        switch (self, other) {
        case (.once(let a), .once(let b)):
            return Self.instant(a, fractional: false).map { $0 == Self.instant(b, fractional: false) } == true
        default: return self == other
        }
    }

    static func instant(_ text: String, fractional: Bool = true) -> Date? {
        guard text.utf8.count <= 40 else { return nil }
        let fraction = fractional ? #"(?:\.[0-9]{1,9})?"# : ""
        let pattern = #"^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}"# + fraction + #"(?:Z|[+-][0-9]{2}:[0-9]{2})$"#
        guard let match = text.range(of: pattern, options: .regularExpression),
              match == text.startIndex..<text.endIndex else { return nil }
        // Foundation's ISO parser normalizes dates such as February 30 on
        // some platforms. Refuse them rather than reviewing a different day.
        let bytes = Array(text.utf8)
        func number(_ start: Int, _ count: Int) -> Int {
            bytes[start..<(start + count)].reduce(0) { $0 * 10 + Int($1 - 48) }
        }
        let year = number(0, 4), month = number(5, 2), day = number(8, 2)
        let leap = year.isMultiple(of: 4) && (!year.isMultiple(of: 100) || year.isMultiple(of: 400))
        let lengths = [31, leap ? 29 : 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
        guard year > 0, (1...12).contains(month), (1...lengths[month - 1]).contains(day),
              number(11, 2) < 24, number(14, 2) < 60, number(17, 2) < 60 else { return nil }
        if bytes.last != 90 {
            guard number(bytes.count - 5, 2) < 24, number(bytes.count - 2, 2) < 60 else { return nil }
        }
        let format = ISO8601DateFormatter()
        format.formatOptions = text.contains(".") ? [.withInternetDateTime, .withFractionalSeconds] : [.withInternetDateTime]
        return format.date(from: text)
    }
}

/// The complete stored ScheduleDto. IDs stay Int64, and existing room/alias
/// references are displayed verbatim, not guessed from the selected household.
public struct SavedSchedule: Codable, Equatable, Sendable, Identifiable {
    public let id: Int64
    public let when: String
    public let action: String
    public let zone: String?
    public let scene: String?
    public let mood: String?
    public let direction: DJDirection?
    public let volume: Int?
    public let fadeSecs: UInt32?
    public let summary: String
    public let creator: String
    public let enabled: Bool
    public let next: String?
    public let lastFired: String?

    public init(id: Int64, when: String, action: String, zone: String?, scene: String?, mood: String?,
                volume: Int?, fadeSecs: UInt32?, summary: String, creator: String, enabled: Bool,
                next: String?, lastFired: String?, direction: DJDirection? = nil) {
        self.id = id; self.when = when; self.action = action; self.zone = zone; self.scene = scene
        self.mood = mood; self.direction = direction; self.volume = volume; self.fadeSecs = fadeSecs
        self.summary = summary; self.creator = creator; self.enabled = enabled
        self.next = next; self.lastFired = lastFired
    }

    public var stateLabel: String { !enabled ? "Paused" : (next == nil ? "No next run reported" : "Enabled") }
    public var targetLabel: String { scene.map { "Scene: " + $0 + " (house-wide)" } ?? zone ?? "Unspecified" }
    public var djSummary: String? {
        guard action == "dj_start" else { return nil }
        return direction?.summary(mood: mood) ?? mood.map { "DJ mood: " + $0 } ?? "Start the DJ with its existing direction"
    }

    public func validated() throws -> Self {
        guard id > 0, scheduleText(when, limit: 128), scheduleText(summary), scheduleText(creator),
              next.map({ ScheduleTime.instant($0) != nil }) ?? true,
              lastFired.map({ ScheduleTime.instant($0) != nil }) ?? true,
              enabled || next == nil else { throw ContractError.invalid("The schedule response is incomplete or contradictory.") }
        _ = try ScheduleTime.stored(when)
        guard action == "dj_start" || direction == nil else {
            throw ContractError.invalid("A saved DJ direction is attached to another scheduled action.")
        }
        _ = try direction?.validated(mood: mood)
        let valid: Bool
        switch action {
        case "dj_start":
            valid = zone.map(scheduleText) == true && scene == nil && volume == nil && fadeSecs == nil && (mood.map(scheduleText) ?? true)
        case "pause":
            valid = zone.map(scheduleText) == true && scene == nil && mood == nil && volume == nil && (fadeSecs ?? 0) <= 600
        case "volume":
            valid = zone.map(scheduleText) == true && scene == nil && mood == nil && fadeSecs == nil && volume.map({ (0...100).contains($0) }) == true
        case "apply_scene":
            valid = scene.map(scheduleText) == true && zone == nil && mood == nil && volume == nil && fadeSecs == nil
        default: valid = false
        }
        guard valid else { throw ContractError.invalid("The schedule action is incomplete or unsupported.") }
        return self
    }

    /// Firings change next/last_fired without changing the owner's definition.
    /// This is an inferred fingerprint, not a daemon revision or transaction.
    public func sameDefinition(as other: Self, includingEnabled: Bool = true) -> Bool {
        id == other.id && when == other.when && creator == other.creator &&
        fields == other.fields && (!includingEnabled || enabled == other.enabled)
    }

    var fields: [String: JSONValue] {
        var fields: [String: JSONValue] = ["action": .string(action)]
        if let zone { fields["zone"] = .string(zone) }
        if let scene { fields["scene"] = .string(scene) }
        if let mood { fields["mood"] = .string(mood) }
        if let direction { fields["direction"] = .object(direction.fields) }
        if let volume { fields["volume"] = .integer(Int64(volume)) }
        if let fadeSecs, fadeSecs > 0 { fields["fade_secs"] = .integer(Int64(fadeSecs)) }
        return fields
    }
}

public enum ScheduledAction: Equatable, Sendable {
    case djStart(room: RoomID, mood: String?)
    case pause(room: RoomID, fadeSecs: UInt32)
    case volume(room: RoomID, level: Int)
    case scene(Scene)

    public var room: RoomID? {
        switch self { case .djStart(let room, _), .pause(let room, _), .volume(let room, _): room; case .scene: nil }
    }
    public var savedScene: Scene? { if case .scene(let scene) = self { scene } else { nil } }
    public var description: String {
        switch self {
        case .djStart(let room, let mood): "Start the DJ at \(room.address)" + (mood.map { " (mood: \($0))" } ?? "")
        case .pause(let room, let fade): "Pause \(room.address)" + (fade > 0 ? " after a \(fade)-second fade" : "")
        case .volume(let room, let level): "Set \(room.address) to volume \(level)"
        case .scene(let scene): "Apply scene \(scene.name) throughout its saved house scope"
        }
    }

    func fields() throws -> [String: JSONValue] {
        if let room, !scheduleText(room.household) || !scheduleText(room.room) {
            throw ContractError.invalid("Choose an exact room and household for this schedule.")
        }
        switch self {
        case .djStart(let room, let mood):
            var fields: [String: JSONValue] = ["action": .string("dj_start"), "zone": .string(room.address)]
            if let mood {
                let name = mood.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
                guard scheduleText(name) else { throw ContractError.invalid("Choose a complete DJ mood name.") }
                fields["mood"] = .string(name)
            }
            return fields
        case .pause(let room, let fade):
            guard fade <= 600 else { throw ContractError.invalid("A scheduled pause fade must be at most 600 seconds.") }
            var fields: [String: JSONValue] = ["action": .string("pause"), "zone": .string(room.address)]
            if fade > 0 { fields["fade_secs"] = .integer(Int64(fade)) }
            return fields
        case .volume(let room, let level):
            guard (0...100).contains(level) else { throw ContractError.invalid("Scheduled volume must be between 0 and 100.") }
            return ["action": .string("volume"), "zone": .string(room.address), "volume": .integer(Int64(level))]
        case .scene(let scene):
            _ = try scene.validated()
            return ["action": .string("apply_scene"), "scene": .string(scene.name)]
        }
    }
}

public struct ScheduleDraft: Equatable, Sendable {
    public let time: ScheduleTime
    public let action: ScheduledAction
    public init(time: ScheduleTime, action: ScheduledAction) { self.time = time; self.action = action }
    func fields() throws -> [String: JSONValue] {
        var fields = try action.fields()
        fields["when"] = .string(try time.canonical())
        return fields
    }
}

public enum ScheduleChange: String, CaseIterable, Sendable { case pause, resume, remove }

public enum ScheduleCommand: Equatable, Sendable {
    case add(ScheduleDraft)
    case change(SavedSchedule, ScheduleChange)

    public var title: String {
        switch self {
        case .add: "Create schedule"
        case .change(let schedule, let change): "\(change.rawValue.capitalized) schedule #\(schedule.id)"
        }
    }
    public var detail: String {
        switch self {
        case .add(let draft): "\((try? draft.time.canonical()) ?? "Invalid time"): \(draft.action.description)"
        case .change(let schedule, _):
            ["#\(schedule.id) • \(schedule.when)", schedule.summary, schedule.djSummary,
             schedule.targetLabel, "Runs as: \(schedule.creator)"].compactMap { $0 }.joined(separator: "\n")
        }
    }

    func payload() throws -> (path: [String], body: [String: JSONValue]) {
        switch self {
        case .add(let draft): return (["schedules"], try draft.fields())
        case .change(let schedule, let change):
            _ = try schedule.validated()
            return (["schedules", change.rawValue], ["id": .integer(schedule.id)])
        }
    }

    /// Store operations do not need speaker playback/volume reads. Creation
    /// verifies its explicit room or saved scene; management verifies only the
    /// stored ID and definition so an offline room cannot prevent cancellation.
    func preflight(_ client: DaemonClient) async throws -> SavedSchedule? {
        try Task.checkCancellation()
        _ = try payload()
        switch self {
        case .add(let draft):
            if let room = draft.action.room {
                let zones = try await client.zones()
                guard zones.contains(where: { $0.rooms.contains(room) }) else {
                    throw ContractError.invalid("The scheduled room is no longer in its household. Refresh rooms.")
                }
            }
            if let scene = draft.action.savedScene {
                guard try await client.scene(named: scene.name) == scene else {
                    throw ContractError.invalid("The scheduled scene changed. Read and review its definition again.")
                }
            }
            try Task.checkCancellation()
            return nil
        case .change(let expected, _):
            let current = try await client.schedules().first { $0.id == expected.id }
            guard let current, expected.sameDefinition(as: current) else {
                throw ContractError.invalid("The reviewed schedule changed or was removed. Refresh schedules.")
            }
            try Task.checkCancellation()
            return current
        }
    }

    func receipt(_ data: Data) throws -> ScheduleReceipt {
        let actual = try DaemonClient.decode(SavedSchedule.self, from: data).validated()
        let title: String
        switch self {
        case .add(let draft):
            guard actual.enabled, try actual.fields == draft.action.fields(),
                  try draft.time.sameTime(as: ScheduleTime.stored(actual.when)) else { throw ClientError.unreadable }
            title = "Created schedule #\(actual.id)"
        case .change(let expected, let change):
            guard expected.sameDefinition(as: actual, includingEnabled: false) else { throw ClientError.unreadable }
            switch change {
            case .pause: guard !actual.enabled else { throw ClientError.unreadable }
            case .resume: guard actual.enabled else { throw ClientError.unreadable }
            case .remove: guard actual.enabled == expected.enabled else { throw ClientError.unreadable }
            }
            title = "\(change == .remove ? "Removed" : change == .pause ? "Paused" : "Resumed") schedule #\(actual.id)"
        }
        let removed = if case .change(_, .remove) = self { true } else { false }
        let status = removed ? "This definition was removed; the response describes what it was." :
            actual.next.map { "Next run reported by the daemon: " + $0 } ?? actual.stateLabel
        let detail = [actual.when, actual.summary, actual.djSummary, actual.targetLabel, "Runs as: " + actual.creator,
                      status, "This confirms a schedule change, not successful playback. An already-started run may still finish."].compactMap { $0 }.joined(separator: "\n")
        return ScheduleReceipt(schedule: actual, title: title, detail: detail, removed: removed)
    }
}

public struct ScheduleReview: Identifiable, Sendable {
    public let id: UUID
    public let command: ScheduleCommand
    public let current: SavedSchedule?
    public let observedAt: Date
    // Only the shared session can issue a usable review: its pending UUID is
    // private and consumed before the first await of either execution path.
    init(command: ScheduleCommand, current: SavedSchedule?) {
        id = UUID(); self.command = command; self.current = current; observedAt = Date()
    }
}

public struct ScheduleReceipt: Sendable {
    public let schedule: SavedSchedule
    public let title: String
    public let detail: String
    public let removed: Bool
}

extension DaemonClient {
    public func schedules() async throws -> [SavedSchedule] {
        let schedules = try await get([SavedSchedule].self, ["schedules"])
        guard schedules.count <= 4_096, Set(schedules.map(\.id)).count == schedules.count else {
            throw ContractError.invalid("The schedule list is ambiguous or too large. Refresh schedules.")
        }
        return try schedules.map { try $0.validated() }
    }
}

private func scheduleText(_ value: String) -> Bool { scheduleText(value, limit: 4_096) }
private func scheduleText(_ value: String, limit: Int) -> Bool {
    !value.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty && value.utf8.count <= limit &&
    !value.unicodeScalars.contains(where: CharacterSet.controlCharacters.contains)
}
