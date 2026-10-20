import Foundation

/// Display time comes from this room's last observation. It never advances a
/// queue or confirms a command, and stops estimating while live state is stale.
public struct PlaybackTimeline: Equatable, Sendable {
    public let duration: UInt32?
    public let observedAt: Date
    public let isPlaying: Bool
    public let isFresh: Bool
    public let canSeek: Bool
    private let reportedPosition: UInt32?
    private let frozenAt: Date?

    public init(track: Track?, isPlaying: Bool, observedAt: Date, isFresh: Bool,
                allowsSeeking: Bool = true, frozenAt: Date? = nil) {
        let duration = track?.durationSecs.flatMap { $0 > 0 ? $0 : nil }
        let position = track?.positionSecs
        self.duration = duration
        reportedPosition = position
        self.observedAt = observedAt
        self.isPlaying = isPlaying
        self.isFresh = isFresh
        self.frozenAt = frozenAt
        canSeek = isFresh && allowsSeeking && duration != nil && position != nil
    }

    public func position(at date: Date) -> TimeInterval? {
        guard let reportedPosition else { return nil }
        let observed = TimeInterval(reportedPosition)
        guard let duration else { return observed }
        let estimateUntil = isFresh ? date : frozenAt ?? observedAt
        let elapsed = estimateUntil.timeIntervalSince(observedAt)
        let advance = isPlaying && elapsed.isFinite ? max(0, elapsed) : 0
        return min(TimeInterval(duration), observed + advance)
    }

    public func elapsedLabel(at date: Date) -> String {
        position(at: date).map(Self.format) ?? "–:––"
    }

    public func remainingLabel(at date: Date) -> String {
        guard let duration, let position = position(at: date) else { return "–:––" }
        return "−" + Self.format(max(0, TimeInterval(duration) - position))
    }

    /// The daemon accepts positions strictly before the track's end. Sliding
    /// all the way right therefore chooses its last second, never a bad seek.
    public func seekPosition(fraction: Double) -> UInt32? {
        guard canSeek, let duration, fraction.isFinite else { return nil }
        let progress = min(1, max(0, fraction))
        return min(duration - 1, UInt32((TimeInterval(duration) * progress).rounded(.down)))
    }

    public static func format(_ seconds: TimeInterval) -> String {
        guard seconds.isFinite else { return "–:––" }
        let whole = UInt64(min(TimeInterval(UInt32.max), max(0, seconds)).rounded(.down))
        let minutes = whole / 60, remainder = whole % 60
        if whole >= 3_600 {
            return "\(whole / 3_600):\(twoDigits(minutes % 60)):\(twoDigits(remainder))"
        }
        return "\(minutes):\(twoDigits(remainder))"
    }

    private static func twoDigits(_ value: UInt64) -> String { value < 10 ? "0\(value)" : String(value) }
}
