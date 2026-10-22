import Foundation

/// A one-use review issued by one CommandReviewSession. Constructing a command
/// or selecting a room does not send it. The UI presents these observations,
/// then explicitly confirms this exact review, not whatever is selected later.
public struct CommandReview: Identifiable, Sendable {
    public let id: UUID
    public let command: NativeCommand
    public let zone: Zone
    public let state: ZoneState
    public let dj: DJStatus?
    public let favorite: Favorite?
    public let sleepTimer: SleepTimer?
    public let observedAt: Date
    fileprivate let observation: CommandObservation
}

/// The write side of a native remote. Observations are read before review and
/// again before confirmation. Only one operation runs at a time, and a review
/// is consumed before any await in execute. There is no write retry or offline
/// queue. Client-side checks are not atomic reservations on the daemon.
public actor CommandReviewSession {
    private let client: DaemonClient
    private var pending: UUID?
    private var generation: UInt64 = 0
    private var busy = false

    public init(client: DaemonClient) { self.client = client }

    /// Invalidate an unsent review, including one still being read. The caller
    /// should also cancel its task to stop I/O. An already-sent action can still
    /// return its receipt; cancellation cannot undo it or prove it had no effect.
    public func cancel() { generation &+= 1; pending = nil }

    public func prepare(_ command: NativeCommand, in zone: Zone) async throws -> CommandReview {
        guard !busy else { throw ContractError.invalid("An action is still being checked or sent.") }
        busy = true
        defer { busy = false }
        generation &+= 1; pending = nil
        let token = generation
        _ = try command.payload(in: zone)
        let observed = try await CommandObservation.read(client, command: command, zone: zone)
        try check(token)
        let review = CommandReview(id: UUID(), command: command, zone: zone,
                                   state: try observed.state(for: command, in: zone),
                                   dj: observed.dj, favorite: observed.favorite, sleepTimer: observed.sleepTimer,
                                   observedAt: Date(), observation: observed)
        pending = review.id
        return review
    }

    public func execute(_ review: CommandReview) async throws -> CommandReceipt {
        guard !busy, pending == review.id else {
            throw ContractError.invalid("This review is no longer current. Review the action again.")
        }
        pending = nil; busy = true
        defer { busy = false }
        let token = generation
        let current = try await CommandObservation.read(client, command: review.command, zone: review.zone)
        try review.observation.validate(current, command: review.command, zone: review.zone)
        try check(token)
        return try await client.perform(review.command, in: review.zone)
    }

    private func check(_ token: UInt64) throws {
        try Task.checkCancellation()
        guard generation == token else { throw CancellationError() }
    }
}

// Scene store changes share this actor's single pending review and write gate.
// They are house-scoped and never fabricate a selected playback destination.
extension CommandReviewSession {
    public func prepare(_ command: SceneMutation) async throws -> SceneMutationReview {
        guard !busy else { throw ContractError.invalid("An action is still being checked or sent.") }
        busy = true
        defer { busy = false }
        generation &+= 1; pending = nil
        let token = generation
        let observed = try await SceneMutationObservation.read(command, client: client)
        try check(token)
        let review = SceneMutationReview(command: command, observation: observed)
        pending = review.id
        return review
    }

    public func execute(_ review: SceneMutationReview) async throws -> SceneMutationReceipt {
        guard !busy, pending == review.id else {
            throw ContractError.invalid("This review is no longer current. Review the scene change again.")
        }
        pending = nil; busy = true
        defer { busy = false }
        let token = generation
        let current = try await SceneMutationObservation.read(review.command, client: client)
        try review.observation.validate(current)
        try check(token)
        return try await client.perform(review)
    }
}

fileprivate struct CommandObservation: Sendable {
    let house: HouseSnapshot
    let dj: DJStatus?
    let favorite: Favorite?
    let scene: Scene?
    let sleepTimer: SleepTimer?

    func state(for command: NativeCommand, in zone: Zone) throws -> ZoneState {
        let id = RoomID(household: zone.household, room: command.room ?? zone.coordinatorRoom)
        guard let value = house.states[id] else {
            throw ContractError.invalid("The selected room could not be read. Nothing was sent.")
        }
        return value
    }

    static func read(_ client: DaemonClient, command: NativeCommand, zone: Zone) async throws -> Self {
        try Task.checkCancellation()
        let zones = try await client.zones()
        guard zones.contains(where: { $0.sameGroup(as: zone) }) else {
            throw ContractError.invalid("The selected group changed. Review its current membership again.")
        }
        try command.validateTopology(reviewed: zone, current: zones)
        let relevant = affectedZones(command, zone: zone, zones: zones)
        let rooms: Set<RoomID>
        if command.requiresFreshTopology || command.savedScene != nil {
            rooms = Set(relevant.flatMap(\.rooms))
        } else if case .groupVolume = command {
            rooms = zone.rooms
        } else {
            rooms = [RoomID(household: zone.household, room: command.room ?? zone.coordinatorRoom)]
        }
        let empty = HouseSnapshot(zones: relevant, states: [:], failures: [:], observedAt: Date())
        let house = try await client.refresh(empty, rooms: rooms)
        guard house.isComplete else {
            let failures = house.failures.sorted { $0.key.address < $1.key.address }
                .map { "\($0.key.address): \($0.value)" }.joined(separator: "\n")
            throw ContractError.invalid("Some affected rooms could not be read. Nothing was sent.\n" + failures)
        }
        let sleepTimer = command.sleepChange != nil ? try await client.sleepTimer(zone) : nil
        try command.sleepChange?.validate(timer: sleepTimer)
        let dj = command.requiresDJ ? try await client.dj(zone) : nil
        if let change = command.steeringChange {
            guard let dj else { throw ContractError.invalid("The DJ steering could not be read.") }
            try change.validate(dj)
            if change.mood != nil {
                let catalog = try await client.get(MoodCatalog.self, ["dj", "moods"], query: [
                    URLQueryItem(name: "zone", value: zone.id.address)
                ])
                try change.validateMoods(catalog.moods)
            }
        }
        var favorite: Favorite?
        if case .playFavorite(let key) = command {
            let key = key.trimmingCharacters(in: .whitespacesAndNewlines)
            let all = try await client.favorites(zone)
            let exact = all.filter { $0.id == key }
            let matches = exact.isEmpty ? all.filter { $0.title == key } : exact
            guard matches.count == 1, matches[0].playable else {
                throw ContractError.invalid("This favorite is missing, ambiguous, or unplayable. Refresh favorites.")
            }
            favorite = matches[0]
        } else if case .queueFavorite(let selection) = command {
            favorite = try selection.validate(await client.favorites(zone), in: zone)
        }
        var scene: Scene?
        if let expected = command.savedScene {
            let current = try await client.get(Scene.self, ["scenes", expected.name])
            guard current == expected else {
                throw ContractError.invalid("The saved scene changed. Read and review it again.")
            }
            scene = current
        }
        switch command {
        case .queuePlay(let item, let revision), .queueRemove(let item, let revision):
            guard item.position > 0 else { throw ContractError.invalid("Queue positions start at one.") }
            let page = try await client.queue(zone, offset: item.position - 1)
            guard page.revision == revision, let found = page.items.first,
                  found.position == item.position, found.uri == item.uri, found.playable else {
                throw ContractError.invalid("The reviewed queue entry changed. Refresh the queue.")
            }
        case .queueMove(let move):
            try move.validate(await client.queue(zone, offset: move.item.position - 1))
        case .listeningSettings(let change):
            try await change.preflight(client, in: zone)
        default: break
        }
        try Task.checkCancellation()
        let result = Self(house: house, dj: dj, favorite: favorite, scene: scene, sleepTimer: sleepTimer)
        let state = try result.state(for: command, in: zone)
        try command.validate(before: state, current: state)
        return result
    }

    func validate(_ current: Self, command: NativeCommand, zone: Zone) throws {
        try command.validate(before: state(for: command, in: zone), current: current.state(for: command, in: zone))
        if command.sleepChange != nil {
            switch (sleepTimer, current.sleepTimer) {
            case (nil, nil): break
            case (.some(let before), .some(let after)) where before.sameTimer(as: after): break
            default: throw ContractError.invalid("The reviewed sleep timer changed. Review its current deadline again.")
            }
        }
        if let change = command.steeringChange {
            guard let dj = current.dj else { throw ContractError.invalid("The DJ steering could not be read.") }
            try change.validate(dj)
        } else if let dj, current.dj.map({ dj.sameSession(as: $0) }) != true {
            throw ContractError.invalid("The DJ selection or steering changed. Review the DJ again.")
        }
        guard favorite == current.favorite, scene == current.scene else {
            throw ContractError.invalid("The reviewed favorite or scene changed. Read it again.")
        }
        if command.requiresFreshTopology || command.savedScene != nil {
            guard Self.sameLayout(house.zones, current.house.zones) else {
                throw ContractError.invalid("An affected group changed. Review the house layout again.")
            }
            for (id, before) in house.states {
                guard let after = current.house.states[id], before.samePlayback(as: after),
                      before.volume == after.volume, before.mute == after.mute else {
                    throw ContractError.invalid("An affected room changed: \(id.address). Review the action again.")
                }
            }
        } else if case .groupVolume = command {
            // Equal averages do not establish equal room levels.
            for (id, before) in house.states {
                guard before.volume != nil, before.volume == current.house.states[id]?.volume else {
                    throw ContractError.invalid("A group member's level changed: \(id.address). Review the levels again.")
                }
            }
        }
    }

    private static func sameLayout(_ left: [Zone], _ right: [Zone]) -> Bool {
        left.count == right.count && left.allSatisfy { a in right.contains { a.sameGroup(as: $0) } }
    }

    private static func affectedZones(_ command: NativeCommand, zone: Zone, zones: [Zone]) -> [Zone] {
        if command.savedScene != nil { return zones }
        if case .party = command { return zones.filter { $0.household == zone.household } }
        let destination: RoomID?
        switch command {
        case .join(_, let to), .move(let to, _): destination = to
        default: destination = nil
        }
        return zones.filter { candidate in
            candidate.sameGroup(as: zone) || destination.map { candidate.rooms.contains($0) } == true
        }
    }
}

// Schedules use this same actor, pending-review slot and write gate as playback.
// Their separate observation type avoids fabricating playback for store-only
// actions or requiring a reachable speaker to pause a future scheduled action.
extension CommandReviewSession {
    public func prepare(_ command: ScheduleCommand) async throws -> ScheduleReview {
        guard !busy else { throw ContractError.invalid("An action is still being checked or sent.") }
        busy = true
        defer { busy = false }
        generation &+= 1; pending = nil
        let token = generation
        let current = try await command.preflight(client)
        try check(token)
        let review = ScheduleReview(command: command, current: current)
        pending = review.id
        return review
    }

    public func execute(_ review: ScheduleReview) async throws -> ScheduleReceipt {
        guard !busy, pending == review.id else {
            throw ContractError.invalid("This review is no longer current. Review the action again.")
        }
        pending = nil; busy = true
        defer { busy = false }
        let token = generation
        _ = try await review.command.preflight(client)
        try check(token)
        return try await client.perform(review.command)
    }
}

// Recovery shares playback and schedule ownership. It does not require a
// selected or reachable speaker: the daemon owns eligibility and restore scope.
extension CommandReviewSession {
    public func prepareUndo() async throws -> UndoReview {
        guard !busy else { throw ContractError.invalid("An action is still being checked or sent.") }
        busy = true
        defer { busy = false }
        generation &+= 1; pending = nil
        let token = generation
        guard let action = try await client.undoPreview() else {
            throw ContractError.invalid("There is no recent action available to undo for this connection.")
        }
        try check(token)
        let review = UndoReview(action: action, observedAt: Date())
        pending = review.id
        return review
    }

    public func execute(_ review: UndoReview) async throws -> UndoReceipt {
        guard !busy, pending == review.id else {
            throw ContractError.invalid("This review is no longer current. Review the available undo again.")
        }
        pending = nil; busy = true
        defer { busy = false }
        let token = generation
        guard let current = try await client.undoPreview(), current == review.action else {
            throw ContractError.invalid("The available undo changed. Read and review the current action again. Nothing was sent.")
        }
        try check(token)
        return try await client.perform(review)
    }
}

// Standing taste shares the same one-use review and busy slot as playback,
// scenes, schedules and undo, but never needs a selected or reachable speaker.
extension CommandReviewSession {
    public func prepare(_ command: DJPreferenceCommand) async throws -> DJPreferenceReview {
        guard !busy else { throw ContractError.invalid("An action is still being checked or sent.") }
        busy = true
        defer { busy = false }
        generation &+= 1; pending = nil
        let token = generation
        try command.validate(await client.djPreferences())
        try check(token)
        let review = DJPreferenceReview(command: command, observedAt: Date())
        pending = review.id
        return review
    }

    public func execute(_ review: DJPreferenceReview) async throws -> DJPreferenceReceipt {
        guard !busy, pending == review.id else {
            throw ContractError.invalid("This review is no longer current. Review standing taste again.")
        }
        pending = nil; busy = true
        defer { busy = false }
        let token = generation
        try review.command.validate(await client.djPreferences())
        try check(token)
        return try await client.perform(review)
    }
}
