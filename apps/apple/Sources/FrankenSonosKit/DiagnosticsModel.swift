import Foundation

/// Explicit, read-only diagnosis independent of speaker selection and SSE.
/// Visibility never starts a check, retries a failure, or executes a remedy.
@MainActor public final class DiagnosticsModel {
    public typealias Reader = @Sendable () async throws -> DiagnosticsReport
    public var onChange: (@MainActor () -> Void)?
    public private(set) var active = false { willSet { onChange?() } }
    public private(set) var loading = false { willSet { onChange?() } }
    public private(set) var observation: DiagnosticsObservation? { willSet { onChange?() } }
    public private(set) var problem: String? { willSet { onChange?() } }
    public private(set) var isCurrent = false { willSet { onChange?() } }
    private let read: Reader
    private let now: @Sendable () -> Date
    private var task: Task<Void, Never>?
    private var epoch: UInt64 = 0
    private var stopped = false

    public init(client: DaemonClient) { read = { try await client.diagnostics() }; now = { Date() } }
    public init(read: @escaping Reader, now: @escaping @Sendable () -> Date = { Date() }) {
        self.read = read; self.now = now
    }
    public var canRun: Bool { active && !stopped && !loading }
    public func resume() { guard !stopped else { return }; active = true }

    public func run() {
        guard canRun else { return }
        epoch &+= 1
        let version = epoch, read = read, now = now
        loading = true; isCurrent = false; problem = nil
        task = Task { [weak self] in
            defer { self?.task = nil; self?.loading = false }
            do {
                // Cancellation before the task starts must prevent even a GET.
                try Task.checkCancellation()
                let report = try await read().validated()
                try Task.checkCancellation()
                guard let self, self.active, !self.stopped, self.epoch == version else { return }
                self.observation = DiagnosticsObservation(report: report, receivedAt: now())
                self.isCurrent = true
            } catch {
                guard let self, self.active, !self.stopped, self.epoch == version, !Task.isCancelled else { return }
                self.problem = error.localizedDescription
            }
        }
    }
    public func cancel() {
        guard !stopped else { return }
        invalidate()
        problem = "Stopped waiting for diagnostics. The daemon may finish its read-only checks. No playback command was sent."
    }
    public func suspend() {
        guard !stopped else { return }
        active = false; invalidate()
    }
    public func stop() {
        guard !stopped else { return }
        suspend(); stopped = true; observation = nil; problem = nil
    }
    private func invalidate() {
        epoch &+= 1; isCurrent = false; task?.cancel()
        // Do not release the loading slot until the cancelled read drains.
    }
    deinit { task?.cancel() }
}
