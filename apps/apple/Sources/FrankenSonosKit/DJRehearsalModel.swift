import Foundation

public struct DJRehearsalObservation: Sendable {
    public let request: DJRehearsalRequest
    public let report: DJRehearsal
    public let generatedAt: Date
}

/// One explicit preview at a time. Editing cancels obsolete work, never generates
/// another request. A transport ignoring cancellation must drain before reuse.
@MainActor public final class DJRehearsalModel {
    public typealias Reader = @Sendable (DJRehearsalRequest) async throws -> DJRehearsal
    public var onChange: (@MainActor () -> Void)?
    public private(set) var draft = DJRehearsalDraft() { willSet { onChange?() } }
    public private(set) var zone: Zone? { willSet { onChange?() } }
    public private(set) var active = false { willSet { onChange?() } }
    public private(set) var loading = false { willSet { onChange?() } }
    public private(set) var observation: DJRehearsalObservation? { willSet { onChange?() } }
    public private(set) var problem: String? { willSet { onChange?() } }
    public private(set) var matchesChoices = false { willSet { onChange?() } }
    private let read: Reader
    private let now: @Sendable () -> Date
    private var task: Task<Void, Never>?
    private var epoch: UInt64 = 0
    private var stopped = false
    public init(read: @escaping Reader, now: @escaping @Sendable () -> Date = { Date() }) {
        self.read = read; self.now = now
    }
    public var canGenerate: Bool { active && !stopped && !loading && zone != nil }
    public var draftProblem: String? {
        guard let zone else { return "Choose a group whose history the DJ should use." }
        do { _ = try draft.request(for: zone); return nil } catch { return error.localizedDescription }
    }
    public func activate(in zone: Zone?) {
        guard !stopped else { return }
        let changed = self.zone.map { before in zone.map { !before.sameGroup(as: $0) } ?? true } ?? (zone != nil)
        if changed { invalidate(); observation = nil; problem = nil }
        self.zone = zone
        active = true
    }
    public func update(_ draft: DJRehearsalDraft) {
        guard !stopped, self.draft != draft else { return }
        invalidate(); self.draft = draft; problem = nil
    }
    public func generate() {
        guard canGenerate, let zone else { return }
        let request: DJRehearsalRequest
        do { request = try draft.request(for: zone) }
        catch { problem = error.localizedDescription; return }
        epoch &+= 1
        let version = epoch, read = read, now = now
        loading = true; problem = nil; matchesChoices = false
        task = Task { [weak self] in
            do {
                try Task.checkCancellation()
                let report = try await read(request).validated(for: request)
                try Task.checkCancellation()
                if let self, self.active, !self.stopped, self.epoch == version,
                   self.zone?.sameGroup(as: request.zone) == true {
                    self.observation = DJRehearsalObservation(request: request, report: report, generatedAt: now())
                    self.matchesChoices = true
                }
            } catch {
                if let self, self.active, !self.stopped, self.epoch == version, !Task.isCancelled {
                    self.problem = error.localizedDescription
                }
            }
            self?.task = nil; self?.loading = false
        }
    }
    public func cancel() {
        invalidate()
        problem = "Preview cancelled. No music was played or changed."
    }
    public func suspend() {
        guard !stopped else { return }
        invalidate(); active = false
    }
    public func stop() { suspend(); stopped = true }
    private func invalidate() {
        epoch &+= 1; matchesChoices = false; task?.cancel()
    }
    deinit { task?.cancel() }
}
