import Foundation

/// The daemon's SleepTimerDto, not an OutcomeDto or a local alarm. A nil
/// deadline is a cancellation receipt; an empty GET list means no timer.
public struct SleepTimer: Codable, Equatable, Sendable {
    public let zone: String
    public let ends: String?
    public let remainingSecs: UInt64?
    public let fades: Bool
    public let fading: Bool
    public let done: String
    public let changed: Bool

    public var deadline: Date? {
        guard let ends else { return nil }
        let format = ISO8601DateFormatter()
        format.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        if let date = format.date(from: ends) { return date }
        format.formatOptions = [.withInternetDateTime]
        return format.date(from: ends)
    }
    public var isRunning: Bool { ends != nil }

    /// A display estimate only. Reaching zero does not prove playback paused.
    public func estimatedRemaining(at now: Date) -> UInt64? {
        guard let end = deadline else { return nil }
        return UInt64(exactly: max(0, end.timeIntervalSince(now).rounded(.up)))
    }

    public func validated(for expected: Zone? = nil) throws -> Self {
        guard !zone.isEmpty, !done.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              expected.map({ zone == $0.coordinatorRoom }) ?? true,
              (ends == nil) == (remainingSecs == nil),
              ends == nil || deadline != nil,
              !fading || (fades && isRunning) else {
            throw ContractError.invalid("The sleep timer response is incomplete or belongs to another group.")
        }
        return self
    }

    /// The daemon stores an exact deadline. A speaker-only timer supplies a
    /// whole-second countdown from which the API reconstructs a deadline;
    /// tolerate its sub-second quantization, not a replaced timer. Neither the
    /// changing countdown nor the human-readable status is a revision token.
    public func sameTimer(as other: Self) -> Bool {
        guard zone == other.zone, fades == other.fades, fading == other.fading else { return false }
        switch (deadline, other.deadline) {
        case (nil, nil): return !isRunning && !other.isRunning
        case (.some(let before), .some(let after)):
            return fades ? before == after : abs(before.timeIntervalSince(after)) <= 1
        default: return false
        }
    }
}

public enum SleepChange: Equatable, Sendable {
    case set(seconds: UInt32), extend(seconds: UInt32), cancel
    public static let maximumSeconds: UInt32 = 23 * 60 * 60

    /// The screen accepts whole minutes; arbitrary text never becomes zero,
    /// a default duration, or a cancellation. The typed API also accepts seconds.
    public static func durationSeconds(minutes input: String) -> UInt32? {
        let text = input.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty, text.utf8.allSatisfy({ (48...57).contains($0) }),
              let minutes = UInt32(text), (1...maximumSeconds / 60).contains(minutes) else { return nil }
        return minutes * 60
    }

    public var title: String {
        func span(_ seconds: UInt32) -> String {
            seconds.isMultiple(of: 60) ? "\(seconds / 60) minutes" : "\(seconds) seconds"
        }
        switch self {
        case .set(let seconds): return "Set sleep timer for \(span(seconds))"
        case .extend(let seconds): return "Extend sleep timer by \(span(seconds))"
        case .cancel: return "Cancel sleep timer"
        }
    }

    func fields() throws -> [String: JSONValue] {
        switch self {
        case .set(let seconds), .extend(let seconds):
            guard (1...Self.maximumSeconds).contains(seconds) else {
                throw ContractError.invalid("A sleep timer duration must be between one second and 23 hours.")
            }
            var body: [String: JSONValue] = ["duration": .string("\(seconds)s")]
            if case .extend = self { body["extend"] = .bool(true) }
            return body
        case .cancel: return ["cancel": .bool(true)]
        }
    }

    func validate(timer: SleepTimer?) throws {
        _ = try fields()
        if case .extend(let seconds) = self {
            guard let timer, timer.isRunning, let remaining = timer.remainingSecs else {
                throw ContractError.invalid("There is no sleep timer to extend. Set a new timer instead.")
            }
            guard remaining <= UInt64(Self.maximumSeconds - seconds) else {
                throw ContractError.invalid("The extended sleep timer would exceed 23 hours.")
            }
        }
    }

    func receipt(_ data: Data, in zone: Zone?) throws -> CommandReceipt {
        let timer = try DaemonClient.decode(SleepTimer.self, from: data).validated(for: zone)
        switch self {
        case .cancel: guard !timer.isRunning else { throw ClientError.unreadable }
        case .set, .extend: guard timer.isRunning else { throw ClientError.unreadable }
        }
        let description: String
        if let ends = timer.ends {
            description = "Pause deadline: \(ends). " + (timer.fades ? "Daemon fade enabled." : "Speaker timer; no fade.")
        } else { description = "The group has no sleep timer. This does not resume playback." }
        return CommandReceipt(title: timer.done, detail: timer.done + "\n" + description)
    }
}

extension DaemonClient {
    /// Always qualify the request: the unfiltered endpoint can omit failing
    /// rooms, and its unqualified DTO names are not household identities.
    public func sleepTimer(_ zone: Zone) async throws -> SleepTimer? {
        _ = try zone.validated()
        let timers = try await get([SleepTimer].self, ["sleep"], query: [URLQueryItem(name: "zone", value: zone.id.address)])
        guard timers.count <= 1 else { throw ContractError.invalid("The selected group returned more than one sleep timer.") }
        guard let timer = timers.first else { return nil }
        _ = try timer.validated(for: zone)
        guard timer.isRunning else { throw ContractError.invalid("The running timer has no deadline. Refresh its state.") }
        return timer
    }
}
