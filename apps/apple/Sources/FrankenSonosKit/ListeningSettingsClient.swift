import Foundation

extension DaemonClient {
    public func speakerTone(_ target: RoomID, in group: Zone) async throws -> SpeakerTone {
        _ = try group.validated()
        guard group.rooms.contains(target) else { throw ContractError.invalid("Choose a speaker in this group.") }
        return try await get(SpeakerTone.self, ["zones", target.address, "eq"]).validated(room: target.room)
    }
    public func playbackOrder(in group: Zone) async throws -> QueuePlaybackOrder {
        _ = try group.validated()
        return try await get(QueuePlaybackOrder.self, ["zones", group.id.address, "play-mode"]).validated(in: group)
    }
    public func listeningSettings(_ scope: ListeningSettingsScope) async throws -> ListeningSettingsValue {
        _ = try scope.validated()
        switch scope {
        case .speaker(let group, _): return .speaker(try await speakerTone(scope.target, in: group))
        case .order(let group): return .order(try await playbackOrder(in: group))
        }
    }
}

extension ListeningSettingsChange {
    public var command: NativeCommand { .listeningSettings(self) }
    public var title: String {
        switch self {
        case .speaker(let change): "Tune \(change.target.room)"
        case .order: "Change shuffle & repeat"
        }
    }
    public var explanation: String {
        switch self {
        case .speaker(let change):
            "Speaker: \(change.target.address)\nObserved: \(change.before.summary)\nRequested: \(change.desired.summary)\n\nOnly edited tone controls change. Music, volume, mute and group membership stay unchanged. A failed multi-part adjustment may be partial; it will not be retried. Activity may offer the daemon's reviewed undo."
        case .order(let change):
            "Group: \(change.group.title) · \(change.group.household)\nObserved: \(change.before.mode.summary)\nRequested: \(change.desired.summary)\n\nChanges queue playback order without starting, restarting or reordering its stored entries. Paused music stays paused. Whole-work DJ ownership is rechecked."
        }
    }
    var path: [String] { switch self { case .speaker: ["eq"]; case .order: ["play-mode"] } }
    func fields(in group: Zone) throws -> [String: JSONValue] {
        switch self {
        case .speaker(let change): try change.fields(in: group)
        case .order(let change): try change.fields(in: group)
        }
    }
    /// Called during both preparation and confirmation inside the same native
    /// command gate as playback, grouping, scheduling and undo.
    func preflight(_ client: DaemonClient, in group: Zone) async throws {
        _ = try fields(in: group)
        switch self {
        case .speaker(let change): try change.validate(await client.speakerTone(change.target, in: group))
        case .order(let change): try change.validate(await client.playbackOrder(in: group))
        }
        try Task.checkCancellation()
    }
    func receipt(_ data: Data) throws -> CommandReceipt {
        switch self {
        case .speaker(let change):
            let reply = try DaemonClient.decode(SpeakerToneReply.self, from: data).validated(for: change)
            return CommandReceipt(title: "Speaker sound updated", detail: reply.done)
        case .order(let change):
            let reply = try DaemonClient.decode(PlaybackOrderReply.self, from: data).validated(for: change)
            return CommandReceipt(title: reply.recovered ? "Playback order observed" : reply.changed ? "Playback order updated" : "Playback order already set",
                                  detail: reply.detail)
        }
    }
}
