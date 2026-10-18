import Foundation

/// A read-only house observation, not an atomic reservation for a command.
/// Missing room state is always paired with a failure; it is never replaced
/// with a fabricated zero volume or another household's same-named room.
public struct HouseSnapshot: Sendable {
    public let zones: [Zone]
    public let states: [RoomID: ZoneState]
    public let failures: [RoomID: String]
    public let observedAt: Date
    public var isComplete: Bool { failures.isEmpty }

    /// Event room names are unqualified. Refresh every matching group, with
    /// each member addressed through its own household-qualified RoomID.
    func affected(by names: Set<String>) -> Set<RoomID> {
        Set(zones.filter { $0.members.contains(where: names.contains) }.flatMap(\.rooms))
    }
}

extension DaemonClient {
    /// Read topology, then all member levels and group playback, with at most
    /// four requests in flight. An unreachable room does not cancel healthy
    /// siblings. A failed topology read cannot be mistaken for an empty house.
    public func houseSnapshot() async throws -> HouseSnapshot {
        try Task.checkCancellation()
        let zones = try await zones()
        return try await readRooms(zones: zones, previous: nil, only: nil)
    }

    func refresh(_ snapshot: HouseSnapshot, rooms: Set<RoomID>) async throws -> HouseSnapshot {
        try await readRooms(zones: snapshot.zones, previous: snapshot, only: rooms)
    }

    private func readRooms(zones: [Zone], previous: HouseSnapshot?, only: Set<RoomID>?) async throws -> HouseSnapshot {
        let all = zones.flatMap { zone in zone.members.map { (zone, RoomID(household: zone.household, room: $0)) } }
        guard all.count <= 1_024 else { throw ContractError.invalid("Too many rooms in the house observation.") }
        let targets = all.filter { only == nil || only!.contains($0.1) }
        var states = previous?.states ?? [:]
        var failures = previous?.failures ?? [:]
        try await withThrowingTaskGroup(of: RoomRead.self) { group in
            var pending = targets.makeIterator()
            func read(_ target: (Zone, RoomID)) async throws -> RoomRead {
                try Task.checkCancellation()
                do {
                    let observed = try await state(target.0, room: target.1.room)
                    try Task.checkCancellation()
                    return RoomRead(id: target.1, state: observed, failure: nil)
                } catch {
                    // Cancellation is session lifetime, not a speaker outage.
                    try Task.checkCancellation()
                    if error is CancellationError { throw error }
                    return RoomRead(id: target.1, state: nil, failure: error.localizedDescription)
                }
            }
            for _ in 0..<min(4, targets.count) {
                if let target = pending.next() { group.addTask { try await read(target) } }
            }
            while let observed = try await group.next() {
                try Task.checkCancellation()
                states[observed.id] = observed.state
                failures[observed.id] = observed.failure
                if let target = pending.next() { group.addTask { try await read(target) } }
            }
        }
        try Task.checkCancellation()
        return HouseSnapshot(zones: zones, states: states, failures: failures, observedAt: Date())
    }
}

private struct RoomRead: Sendable {
    let id: RoomID
    let state: ZoneState?
    let failure: String?
}
