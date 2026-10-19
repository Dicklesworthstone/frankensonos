import Foundation

/// An unsent editor draft. Room selection is explicit and remains unchanged
/// when the playback sidebar moves to a different household. Refreshes never
/// clear a draft or turn it into a stored schedule.
public struct ScheduleForm: Equatable, Sendable {
    public enum Action: String, CaseIterable, Sendable { case dj, pause, volume, scene }
    public enum Frequency: String, CaseIterable, Sendable { case weekly, once }
    public var action: Action = .dj
    public var frequency: Frequency = .weekly
    public var days: Set<ScheduleDay> = [.mon, .tue, .wed, .thu, .fri]
    public var time = "07:30"
    public var once = ""
    public var room: RoomID?
    public var scene = ""
    public var mood = ""
    public var volume = "25"
    public var fadeSeconds = "0"
    public init() {}

    public func draft(zones: [Zone], scenes: [Scene]) throws -> ScheduleDraft {
        let timing: ScheduleTime = frequency == .weekly ? .weekly(days: days, at: time) : .once(at: once)
        _ = try timing.canonical()
        let scheduled: ScheduledAction
        if action == .scene {
            let matches = scenes.filter { $0.name == scene }
            guard matches.count == 1 else { throw ContractError.invalid("Choose a currently loaded saved scene.") }
            scheduled = .scene(try matches[0].validated())
        } else {
            guard let room, try Zone.validate(zones).contains(where: { $0.rooms.contains(room) }) else {
                throw ContractError.invalid("Choose an exact room and household for this schedule.")
            }
            switch action {
            case .dj:
                let name = mood.trimmingCharacters(in: .whitespacesAndNewlines)
                scheduled = .djStart(room: room, mood: name.isEmpty ? nil : name)
            case .pause: scheduled = .pause(room: room, fadeSecs: UInt32(try Self.number(fadeSeconds, maximum: 600)))
            case .volume: scheduled = .volume(room: room, level: try Self.number(volume, maximum: 100))
            case .scene: throw ContractError.invalid("Choose a currently loaded saved scene.")
            }
        }
        let draft = ScheduleDraft(time: timing, action: scheduled)
        _ = try draft.fields()
        return draft
    }

    private static func number(_ text: String, maximum: Int) throws -> Int {
        guard !text.isEmpty, text.utf8.allSatisfy({ (48...57).contains($0) }),
              let value = Int(text), (0...maximum).contains(value) else {
            throw ContractError.invalid("Enter a whole number from 0 to \(maximum), without signs or extra text.")
        }
        return value
    }
}

/// Schedule receipts keep their stored target/ID, not a fabricated room or the
/// current sidebar selection. Reading the list cannot acknowledge a lost write.
public struct RemoteScheduleResult: Identifiable, Sendable {
    public let id = UUID()
    public let command: ScheduleCommand
    public let receipt: ScheduleReceipt?
    public let title: String
    public let detail: String
    public let kind: RemoteResult.Kind
}
