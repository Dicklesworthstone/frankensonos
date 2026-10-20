import Foundation

public struct DJPreferencesObservation: Identifiable, Sendable {
    public let id = UUID()
    public let preferences: DJPreferences
    public let observedAt: Date
}

/// Read-only editor ownership. Commands leave through RemoteModel's shared
/// review gate; opening, refreshing, resuming and filtering never write.
@MainActor public final class DJPreferencesModel {
    public var onChange: (@MainActor () -> Void)?
    public private(set) var observation: DJPreferencesObservation?
    public private(set) var active = false
    public private(set) var loading = false
    public private(set) var fresh = false
    public private(set) var problem: String?
    private let read: @Sendable () async throws -> DJPreferences
    private var task: Task<Void, Never>?
    private var generation: UInt64 = 0
    private var pending = false
    private var stopped = false

    public init(read: @escaping @Sendable () async throws -> DJPreferences) { self.read = read }
    public var canEdit: Bool { active && !stopped && fresh && !loading && observation != nil }
    private func publish() { onChange?() }

    public func resume() {
        guard !active && !stopped else { return }
        active = true
        refresh()
    }
    public func suspend() {
        active = false; fresh = false; pending = false; generation &+= 1
        task?.cancel()
        publish()
    }
    public func stop() { stopped = true; suspend() }

    public func refresh() {
        guard active && !stopped else { return }
        fresh = false; pending = true
        generation &+= 1
        task?.cancel()
        startRead()
        publish()
    }

    private func startRead() {
        guard active && !stopped && pending && task == nil else { return }
        pending = false; loading = true; problem = nil
        let version = generation, read = read
        task = Task { [weak self] in
            do {
                try Task.checkCancellation()
                let preferences = try await read()
                try preferences.validate()
                try Task.checkCancellation()
                if let self, self.active && !self.stopped && self.generation == version {
                    self.observation = DJPreferencesObservation(preferences: preferences, observedAt: Date())
                    self.fresh = true
                }
            } catch {
                if let self, self.active && !self.stopped && self.generation == version && !Task.isCancelled {
                    // Preserve the original observation and its time, not edit authority.
                    self.problem = error.localizedDescription
                    self.fresh = false
                }
            }
            guard let self else { return }
            self.task = nil; self.loading = false
            self.startRead()
            self.publish()
        }
    }

    public func command(_ change: DJPreferenceChange, from expected: DJPreferencesObservation) throws -> DJPreferenceCommand {
        guard canEdit, observation?.id == expected.id else {
            throw DJPreferencesError.invalid("This preference selection is stale. Refresh standing taste before editing.")
        }
        return try DJPreferenceCommand(change: change, before: expected.preferences)
    }
}
