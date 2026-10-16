import Foundation

public struct CommandReceipt: Equatable, Sendable {
    public let title: String
    public let detail: String
    public let partial: Bool
    public init(title: String, detail: String, partial: Bool = false) {
        self.title = title; self.detail = detail; self.partial = partial
    }
}

public enum NativeCommand: Equatable, Sendable {
    case pause, resume, next, previous
    case volume(Int, room: String?), groupVolume(Int), mute(Bool, room: String)
    case seek(UInt32)
    case playSource(String), playFavorite(String)
    case queueAdd(String, next: Bool)
    case queuePlay(QueueItem, revision: String)
    case queueRemove(QueueItem, revision: String)
    case djStart(mood: String?), djStop, djSkip, djFeedback(like: Bool)
    case djSteer(mood: String, seconds: UInt32), djClear
    case scene(Scene)

    public var title: String {
        switch self {
        case .pause: "Pause music"
        case .resume: "Resume music"
        case .next: "Next track"
        case .previous: "Previous track"
        case .volume(let n, _): "Set room volume to \(n)"
        case .groupVolume(let n): "Set group volume to \(n)"
        case .mute(let flag, _): flag ? "Mute room" : "Unmute room"
        case .seek: "Seek within this track"
        case .playSource: "Play this source"
        case .playFavorite: "Play this favorite"
        case .queueAdd(_, let next): next ? "Queue next" : "Add to queue"
        case .queuePlay(let item, _): "Play \(item.title)"
        case .queueRemove: "Remove queue entry"
        case .djStart: "Start your DJ"
        case .djStop: "Stop the DJ"
        case .djSkip: "Skip this whole work"
        case .djFeedback(let like): like ? "Like this DJ selection" : "Dislike this DJ selection"
        case .djSteer(let mood, _): "Shift to \(mood)"
        case .djClear: "Follow the time-of-day program"
        case .scene(let scene): "Apply \(scene.name)"
        }
    }
    public var explanation: String {
        switch self {
        case .queueAdd: "Adds music without starting playback. The daemon resolves the source and applies its queue rules."
        case .queueRemove: "Removes this exact revision-bound entry. This cannot be undone; removing the current entry can advance or stop playback."
        case .groupVolume: "Changes every member of this group. The daemon's per-room caps and quiet hours remain authoritative."
        case .volume, .mute: "Changes only the named room. The group's music and membership are unchanged."
        case .djSkip: "Advances to the DJ's next whole work, not merely the next movement."
        case .djSteer, .djClear: "Affects future DJ picks, not works already queued. Standing taste preferences stay unchanged."
        case .djFeedback: "Teaches the DJ about its current selection. It does not send a playback command."
        case .djStop: "Stops DJ queue feeding. This is distinct from pausing the speakers."
        case .scene: "May regroup rooms, replace sources, start or pause music, and change levels and mutes. Review all listed scene settings. Partial outcomes are shown individually."
        case .seek: "Checks the current track before seeking. Does not resume paused music."
        case .playSource, .playFavorite, .djStart: "Replaces what this group plays and can change its queue. The daemon applies playback volume caps."
        default: "Controls the selected group's coordinator. Other households remain independent."
        }
    }
    public var requiresDJ: Bool {
        switch self {
        case .djStart, .djStop, .djSkip, .djFeedback, .djSteer, .djClear: true
        default: false
        }
    }
    public var isQueueAdd: Bool { if case .queueAdd = self { true } else { false } }
    public var room: String? {
        switch self { case .volume(_, let room): room; case .mute(_, let room): room; default: nil }
    }
    public var savedScene: Scene? { if case .scene(let scene) = self { scene } else { nil } }

    struct Payload {
        let path: [String]
        let body: [String: JSONValue]
        let timeout: TimeInterval
    }
    func payload(in zone: Zone) throws -> Payload {
        _ = try zone.validated()
        var path: [String] = [], body: [String: JSONValue] = ["zone": .string(zone.id.address)]
        let timeout: TimeInterval = requiresDJ || savedScene != nil || isQueueAdd ? 600 : 120
        func require(_ flag: Bool, _ message: String) throws { if !flag { throw ContractError.invalid(message) } }
        func source(_ value: String) throws -> String {
            let value = value.trimmingCharacters(in: .whitespacesAndNewlines)
            try require(!value.isEmpty && value.utf8.count <= 4_096 && !value.unicodeScalars.contains(where: CharacterSet.controlCharacters.contains), "Enter a complete music source.")
            return value
        }
        if let room {
            try require(zone.members.contains(room), "The room no longer belongs to this group.")
            body["zone"] = .string(RoomID(household: zone.household, room: room).address)
        }
        switch self {
        case .pause: path = ["pause"]
        case .resume: path = ["resume"]
        case .next: path = ["next"]
        case .previous: path = ["previous"]
        case .volume(let n, _), .groupVolume(let n):
            try require((0...100).contains(n), "Volume must be between 0 and 100.")
            path = ["volume"]; body["volume"] = .integer(Int64(n))
            if case .groupVolume = self { body["group"] = .bool(true) }
        case .mute(let value, _): path = ["mute"]; body["mute"] = .bool(value)
        case .seek(let secs): path = ["seek"]; body["to_secs"] = .integer(Int64(secs))
        case .playSource(let uri): path = ["play"]; body["source_uri"] = .string(try source(uri))
        case .playFavorite(let favorite): path = ["play", "favorite"]; body["favorite"] = .string(try source(favorite))
        case .queueAdd(let uri, let next):
            path = ["queue"]; body["source_uri"] = .string(try source(uri)); body["next"] = .bool(next)
        case .queuePlay(let item, let revision), .queueRemove(let item, let revision):
            try require(item.position > 0 && item.playable && !revision.isEmpty, "Refresh this queue before editing it.")
            body["position"] = .integer(Int64(item.position)); body["expected_revision"] = .string(revision)
            if case .queuePlay = self { path = ["queue", "play"]; body["expected_uri"] = .string(item.uri!) }
            else { path = ["queue", "remove"]; body["expected_uris"] = .array([.string(item.uri!)]) }
        case .djStart(let mood): path = ["dj", "start"]; if let mood { body["mood"] = .string(try source(mood)) }
        case .djStop: path = ["dj", "stop"]
        case .djSkip: path = ["dj", "skip"]
        case .djFeedback(let like): path = ["dj", "feedback"]; body["signal"] = .string(like ? "like" : "dislike")
        case .djSteer(let mood, let seconds):
            try require((60...604_800).contains(seconds), "Choose a steering duration from one minute to one week.")
            path = ["dj", "steer"]; body["mood"] = .string(try source(mood)); body["for_secs"] = .integer(Int64(seconds))
        case .djClear: path = ["dj", "steer"]; body["clear"] = .bool(true)
        case .scene(let scene):
            try require(!scene.name.isEmpty, "The scene has no name.")
            path = ["scenes", scene.name, "apply"]; body = [:]
        }
        return Payload(path: path, body: body, timeout: timeout)
    }

    /// A client preflight is not an atomic reservation against other controllers.
    public func validate(before: ZoneState, current: ZoneState) throws {
        _ = try current.validated(for: before.zone)
        let matches: Bool
        switch self {
        case .volume: matches = before.volume != nil && before.volume == current.volume
        case .groupVolume: matches = before.groupVolume != nil && before.groupVolume == current.groupVolume
        case .mute: matches = before.mute != nil && before.mute == current.mute
        case .queuePlay, .queueRemove, .scene: matches = true // Native queue revision/URI checks remain mandatory.
        case .queueAdd(_, let next): matches = !next || before.samePlayback(as: current)
        default: matches = before.samePlayback(as: current)
        }
        guard matches else { throw ContractError.invalid("The reviewed playback or level changed. Review its current state again.") }
        if case .seek(let seconds) = self {
            guard ["playing", "paused"].contains(current.transportState), let duration = current.track?.durationSecs,
                  duration > 0, seconds < duration, current.track?.positionSecs != nil else {
                throw ContractError.invalid("This source does not report a seekable position.")
            }
        }
    }
    func receipt(_ data: Data) throws -> CommandReceipt {
        switch self {
        case .scene(let scene):
            let report = try DaemonClient.decode(SceneReport.self, from: data)
            guard report.scene == scene.name, !report.done.isEmpty,
                  !report.complete || (report.failed ?? []).isEmpty else { throw ClientError.unreadable }
            let lines = report.steps + (report.failed ?? []).map { "Failed: \($0.step) — \($0.error)" } +
                (report.skipped ?? []).map { "Skipped: \($0)" } + (report.notes ?? []).map(\.detail)
            return CommandReceipt(title: report.complete ? report.done : "Scene partially applied",
                                  detail: ([report.done] + lines).joined(separator: "\n"), partial: !report.complete)
        case .queueAdd:
            let report = try DaemonClient.decode(QueueAdded.self, from: data)
            guard report.first > 0 else { throw ClientError.unreadable }
            return CommandReceipt(title: report.done, detail: "Added \(report.count) entries at position \(report.first).\n" + (report.notes ?? []).map(\.detail).joined(separator: "\n"))
        case .queueRemove(let item, _):
            struct Removed: Decodable { let done: String; let position: UInt32; let count: UInt32; let remaining: UInt32; let undoable: Bool }
            let r = try DaemonClient.decode(Removed.self, from: data)
            guard r.position == item.position, r.count == 1, !r.undoable else { throw ClientError.unreadable }
            return CommandReceipt(title: r.done, detail: "\(r.remaining) queue entries remain. This removal cannot be undone.")
        default:
            let report = try DaemonClient.decode(Outcome.self, from: data)
            guard !report.done.isEmpty else { throw ClientError.unreadable }
            if case .volume = self { guard let n = report.volume, (0...100).contains(n) else { throw ClientError.unreadable } }
            if case .groupVolume = self { guard let n = report.volume, (0...100).contains(n) else { throw ClientError.unreadable } }
            return CommandReceipt(title: report.done, detail: report.detail)
        }
    }
}
