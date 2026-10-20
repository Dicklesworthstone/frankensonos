import Foundation

/// An immutable selection of a local draft. It never sends a command itself.
public struct ListeningSettingsSelection: Sendable {
    public let scope: ListeningSettingsScope
    public let change: ListeningSettingsChange
    fileprivate let version: UUID
}

/// The two native settings panels share one bounded read/draft lifecycle. Each
/// visible panel owns a model; writes belong to the existing command session.
@MainActor public final class ListeningSettingsModel {
    public typealias Reader = @Sendable (ListeningSettingsScope) async throws -> ListeningSettingsValue
    public var onChange: (@MainActor () -> Void)?
    public private(set) var scope: ListeningSettingsScope? { willSet { onChange?() } }
    public private(set) var observed: ListeningSettingsValue? { willSet { onChange?() } }
    public private(set) var draft: ListeningSettingsValue? { willSet { onChange?() } }
    public private(set) var observedAt: Date? { willSet { onChange?() } }
    public private(set) var active = false { willSet { onChange?() } }
    public private(set) var loading = false { willSet { onChange?() } }
    public private(set) var isFresh = false { willSet { onChange?() } }
    public private(set) var problem: String? { willSet { onChange?() } }
    public private(set) var notice: String? { willSet { onChange?() } }
    private let reader: Reader
    private let now: @Sendable () -> Date
    private var epoch: UInt64 = 0
    private var version = UUID()
    private var stopped = false
    private var task: Task<Void, Never>?
    private var pending: (ListeningSettingsScope, UInt64)?

    public init(now: @escaping @Sendable () -> Date = { Date() }, read: @escaping Reader) {
        self.now = now; reader = read
    }
    public var canEdit: Bool { active && !stopped && isFresh && !loading && observed != nil }
    public var hasEdits: Bool { observed != nil && draft != nil && observed != draft }
    public var canRefresh: Bool { active && !stopped && !loading && scope != nil }
    public var tone: SpeakerTone? { if case .speaker(let value) = draft { value } else { nil } }
    public var order: QueuePlaybackOrder? { if case .order(let value) = draft { value } else { nil } }

    /// Playback-clock changes are not scope changes; household/group changes are.
    public func activate(_ requested: ListeningSettingsScope) {
        guard !stopped else { return }
        do { _ = try requested.validated() }
        catch { suspend(); problem = error.localizedDescription; return }
        guard !active || scope != requested else { return }
        if scope != requested { observed = nil; draft = nil; observedAt = nil; notice = nil }
        active = true; scope = requested
        refresh()
    }
    public func refresh() {
        guard active && !stopped, let scope else { return }
        epoch &+= 1; version = UUID(); task?.cancel()
        isFresh = false; loading = true; problem = nil
        pending = (scope, epoch)
        pump()
    }
    public func suspend() {
        active = false; epoch &+= 1; version = UUID(); pending = nil
        task?.cancel(); isFresh = false; loading = false
    }
    public func stop() { suspend(); stopped = true }

    public func editTone(bass: Int? = nil, treble: Int? = nil, loudness: Bool? = nil) {
        guard canEdit, let tone, let scope else { return }
        let next = SpeakerTone(room: tone.room, bass: bass ?? tone.bass,
                               treble: treble ?? tone.treble, loudness: loudness ?? tone.loudness)
        guard let value = try? ListeningSettingsValue.speaker(next).validated(for: scope) else { return }
        version = UUID(); draft = value; notice = nil
    }
    public func editOrder(shuffled: Bool? = nil, repetition: QueuePlaybackMode.Repetition? = nil) {
        guard canEdit, let order else { return }
        let mode = QueuePlaybackMode(shuffled: shuffled ?? order.mode.shuffled,
                                     repetition: repetition ?? order.mode.repetition)
        guard order.allows(mode) else { return }
        version = UUID(); notice = nil
        draft = .order(QueuePlaybackOrder(zone: order.zone, mode: mode,
                                         queueActive: order.queueActive, djActive: order.djActive))
    }
    /// A running DJ may have inherited a non-sequential mode from another
    /// controller. Offer the one safe complete mode, not separate partial toggles.
    public func useSequentialOrder() {
        guard canEdit, let order, order.allows(.normal) else { return }
        version = UUID(); notice = nil
        draft = .order(QueuePlaybackOrder(zone: order.zone, mode: .normal,
                                         queueActive: order.queueActive, djActive: order.djActive))
    }
    public func discard() {
        guard canEdit else { return }
        version = UUID(); draft = observed; notice = nil
    }
    public func selection() -> ListeningSettingsSelection? {
        guard canEdit, hasEdits, let scope else { return nil }
        let change: ListeningSettingsChange
        switch (observed, draft) {
        case (.speaker(let before), .speaker(let desired)):
            guard let edit = try? SpeakerToneChange(target: scope.target, before: before,
                bass: desired.bass, treble: desired.treble, loudness: desired.loudness) else { return nil }
            change = .speaker(edit)
        case (.order(let before), .order(let desired)):
            guard let edit = try? PlaybackOrderChange(group: scope.group, before: before, desired: desired.mode) else { return nil }
            change = .order(edit)
        default: return nil
        }
        return ListeningSettingsSelection(scope: scope, change: change, version: version)
    }
    public func resolve(_ selection: ListeningSettingsSelection) throws -> ListeningSettingsChange {
        guard let current = self.selection(), current.version == selection.version,
              current.scope == selection.scope, current.change == selection.change else {
            throw ContractError.invalid("These sound settings or their destination changed. Review the current controls again.")
        }
        return current.change
    }

    private func pump() {
        guard active && !stopped, task == nil, let (scope, epoch) = pending else { return }
        pending = nil
        let reader = reader
        task = Task { [weak self] in
            defer { self?.finish() }
            do {
                try Task.checkCancellation()
                let value = try await reader(scope).validated(for: scope)
                try Task.checkCancellation()
                guard let self, self.active && self.epoch == epoch && self.scope == scope else { return }
                let keep = self.observed == value && self.hasEdits
                if !keep {
                    self.notice = self.hasEdits ? "The setting changed on the speaker. Unsaved edits were discarded; nothing was sent." : nil
                    self.draft = value
                }
                self.observed = value; self.observedAt = self.now()
                self.isFresh = true; self.loading = false; self.problem = nil; self.version = UUID()
            } catch {
                guard let self, self.active && self.epoch == epoch && self.scope == scope else { return }
                self.loading = false; self.isFresh = false; self.problem = error.localizedDescription
            }
        }
    }
    private func finish() { task = nil; pump() }
}
