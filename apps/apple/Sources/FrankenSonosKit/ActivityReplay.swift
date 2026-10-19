import Foundation

/// A source and an explicitly selected current group. Historical room labels
/// never supply this target. Handoff still needs the ordinary fresh review.
public struct ActivityReplaySelection: Sendable {
    public let command: NativeCommand
    public let zone: Zone

    public func validated(against zones: [Zone]) throws -> Self {
        let zones = try Zone.validate(zones)
        guard zones.contains(where: { $0.sameGroup(as: zone) }) else {
            throw ContractError.invalid("The replay destination changed. Choose its current group again.")
        }
        return self
    }
}

extension ActivityModel {
    public func selectForReplay(_ id: UUID, as mode: Replay, target: Zone?, current: [Zone]) throws -> ActivityReplaySelection {
        let command = try replayCommand(id, as: mode)
        guard let target else { throw ContractError.invalid("Choose a current group and household for this source.") }
        return try ActivityReplaySelection(command: command, zone: target).validated(against: current)
    }
}
