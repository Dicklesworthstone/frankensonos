import Foundation

public struct DJPreferencesResult: Identifiable, Sendable {
    public enum Kind: Sendable { case confirmed, unchanged, uncertain, notSent }
    public let id = UUID()
    public let change: DJPreferenceChange
    public let kind: Kind
    public let detail: String
    public var title: String {
        switch kind {
        case .confirmed: "Standing taste saved"
        case .unchanged: "No preference changed"
        case .uncertain: "Save outcome uncertain — not repeated"
        case .notSent: "Preference was not sent"
        }
    }
}

/// The preference screen uses the connection's existing CommandReviewSession.
/// Its reader is separate from room availability; its command ownership is not.
@MainActor public final class DJPreferencesWorkflow {
    public enum Phase: Sendable { case idle, preparing, confirming }
    public var onChange: (@MainActor () -> Void)?
    public var canBegin: @MainActor () -> Bool = { true }
    public var awaitBarrier: @MainActor () async -> Void = {}
    public let editor: DJPreferencesModel
    public private(set) var active = false
    public private(set) var review: DJPreferenceReview?
    public private(set) var result: DJPreferencesResult?
    public private(set) var phase: Phase = .idle
    public private(set) var needsRefresh = false
    private let commands: CommandReviewSession
    private var task: Task<Void, Never>?
    private var cancellation: Task<Void, Never>?
    private var generation: UInt64 = 0
    private var clearingUncertainty = false
    private var stopped = false
    public var isEngaged: Bool { phase != .idle || task != nil || cancellation != nil || review != nil }
    public var canPrepare: Bool { active && !stopped && !isEngaged && !needsRefresh && canBegin() && editor.canEdit }

    public init(client: DaemonClient, commands: CommandReviewSession) {
        self.commands = commands
        editor = DJPreferencesModel(read: { try await client.djPreferences() })
        editor.onChange = { [weak self] in
            guard let self else { return }
            if self.clearingUncertainty && !self.editor.loading {
                if self.editor.fresh { self.needsRefresh = false; self.clearingUncertainty = false }
                else if self.editor.problem != nil { self.clearingUncertainty = false }
            }
            self.publish()
        }
    }
    private func publish() { onChange?() }
    public func resume() {
        guard !active && !stopped else { return }
        active = true
        if !isEngaged { editor.resume() }
        publish()
    }
    public func suspend() {
        active = false; clearingUncertainty = false
        editor.suspend(); cancel()
        publish()
    }
    public func stop() { stopped = true; suspend(); editor.stop() }

    /// Only an explicit successful refresh clears uncertainty. Merely opening
    /// the panel, changing rooms or foregrounding cannot authorize a replay.
    public func refresh() {
        guard active && !stopped && !isEngaged && canBegin() else { return }
        clearingUncertainty = needsRefresh
        if editor.active { editor.refresh() } else { editor.resume() }
    }

    public func prepare(_ change: DJPreferenceChange, from observation: DJPreferencesObservation) {
        guard canPrepare else { return }
        let command: DJPreferenceCommand
        do { command = try editor.command(change, from: observation) }
        catch {
            result = DJPreferencesResult(change: change, kind: .notSent, detail: error.localizedDescription)
            publish(); return
        }
        generation &+= 1
        let version = generation, commands = commands, awaitBarrier = awaitBarrier
        clearingUncertainty = false; phase = .preparing; result = nil
        editor.suspend()
        task = Task { [weak self] in
            await awaitBarrier()
            do {
                try Task.checkCancellation()
                let reviewed = try await commands.prepare(command)
                if let self, self.active && self.generation == version && !Task.isCancelled {
                    self.review = reviewed
                }
            } catch {
                if let self, self.active && self.generation == version && !Task.isCancelled {
                    self.result = DJPreferencesResult(change: change, kind: .notSent, detail: error.localizedDescription)
                }
            }
            self?.finished()
        }
        publish()
    }

    public func confirm(_ id: UUID) {
        guard active && !stopped && task == nil && cancellation == nil,
              let reviewed = review, reviewed.id == id else { return }
        review = nil; phase = .confirming
        let commands = commands
        task = Task { [weak self] in
            do {
                let receipt = try await commands.execute(reviewed)
                self?.result = DJPreferencesResult(change: reviewed.command.change,
                    kind: receipt.changed ? .confirmed : .unchanged, detail: receipt.done)
            } catch {
                let uncertain: Bool
                if case ClientError.uncertain = error { uncertain = true } else { uncertain = false }
                if uncertain { self?.needsRefresh = true }
                self?.result = DJPreferencesResult(change: reviewed.command.change,
                    kind: uncertain ? .uncertain : .notSent,
                    detail: uncertain
                        ? "This preference may already be saved. It was not repeated. Refresh standing taste before making another change.\n" + error.localizedDescription
                        : error.localizedDescription)
            }
            // A late receipt remains daemon-wide even if the selected room
            // changed, the sheet closed, or the app went into the background.
            self?.finished()
        }
        publish()
    }

    public func cancel() {
        guard isEngaged else { return }
        generation &+= 1; let version = generation
        review = nil; clearingUncertainty = false
        let running = task, previous = cancellation, commands = commands
        running?.cancel()
        cancellation = Task { [weak self] in
            await previous?.value
            await commands.cancel()
            await running?.value
            guard let self, self.generation == version else { return }
            self.cancellation = nil
            if self.task == nil {
                self.phase = .idle
                if self.active && !self.stopped { self.editor.resume() }
            }
            self.publish()
        }
        publish()
    }
    public func dismissResult() { result = nil; publish() }
    private func finished() {
        task = nil; phase = .idle
        if active && !stopped && cancellation == nil && review == nil { editor.resume() }
        publish()
    }
}
