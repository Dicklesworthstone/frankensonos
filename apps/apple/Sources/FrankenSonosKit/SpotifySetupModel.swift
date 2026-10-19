import Foundation

public struct SpotifySetupResult: Sendable {
    public let title: String
    public let detail: String
    public let uncertain: Bool
}

/// Account/cache workflow, not a playback writer. All mutations are explicit
/// user actions; only GETs repeat while sign-in or a library refresh is pending.
/// The configured daemon holds PKCE and tokens. No callback is published or saved.
@MainActor public final class SpotifySetupModel {
    public enum Activity: String, Sendable { case idle, reading, beginning, completing, syncing }
    public var onChange: (@MainActor () -> Void)?
    public private(set) var active = false { willSet { onChange?() } }
    public private(set) var activity: Activity = .idle { willSet { onChange?() } }
    public private(set) var signIn: SpotifySignIn? { willSet { onChange?() } }
    public private(set) var library: SpotifyLibrarySync? { willSet { onChange?() } }
    public private(set) var consent: SpotifyConsent? { willSet { onChange?() } }
    public private(set) var signInProblem: String? { willSet { onChange?() } }
    public private(set) var libraryProblem: String? { willSet { onChange?() } }
    public private(set) var observedAt: Date? { willSet { onChange?() } }
    public private(set) var result: SpotifySetupResult? { willSet { onChange?() } }
    public var canBegin: Bool { active && activity == .idle && signIn != nil }
    public var canComplete: Bool { canBegin && consent != nil && signIn?.pending == true }
    public var canSync: Bool { active && activity == .idle && library != nil && library?.running == false }

    private let client: DaemonClient
    private let timing: Timing
    private var worker: Task<Void, Never>?
    private var timer: Task<Void, Never>?
    private var generation: UInt64 = 0
    private var readPending = false, stopped = false
    private var retry: Duration

    struct Timing: Sendable {
        var poll: Duration = .seconds(2)
        var maximumRetry: Duration = .seconds(60)
    }
    public convenience init(client: DaemonClient) { self.init(client: client, timing: Timing()) }
    init(client: DaemonClient, timing: Timing) {
        self.client = client; self.timing = timing; retry = timing.poll
    }

    public func resume() {
        guard !active && !stopped else { return }
        active = true; generation &+= 1; refresh()
    }
    public func suspend() {
        active = false; generation &+= 1
        timer?.cancel(); timer = nil; readPending = false
        worker?.cancel() // Hold the slot until a cancellation-ignoring transport drains.
    }
    public func stop() {
        suspend(); stopped = true; consent = nil
    }
    public func refresh() {
        guard active && !stopped else { return }
        timer?.cancel(); timer = nil; readPending = true
        startRead()
    }
    public func dismissResult() { result = nil }

    public func beginSignIn(replacePending: Bool = false) {
        guard canBegin, let reviewed = signIn else { return }
        guard !reviewed.pending || replacePending else {
            result = SpotifySetupResult(title: "Sign-in already pending", detail: "Replacing it invalidates the earlier browser flow. Confirm replacement explicitly.", uncertain: false)
            return
        }
        consent = nil
        start(.begin(reviewed, replacePending), activity: .beginning)
    }

    /// The caller clears its text field immediately after this explicit action.
    /// Invalid text never reaches the daemon and never appears in an error.
    public func completeSignIn(_ callback: String) {
        guard canComplete, let consent else { return }
        do {
            let checked = try consent.callback(callback)
            self.consent = nil // Consume this completion before any async work.
            start(.complete(consent, checked), activity: .completing)
        } catch {
            result = SpotifySetupResult(title: "Callback not sent", detail: SpotifySetupError.invalidCallback.localizedDescription, uncertain: false)
        }
    }

    public func syncLibrary() {
        guard canSync else { return }
        start(.sync, activity: .syncing)
    }

    private func startRead() {
        guard active && !stopped && readPending && worker == nil else { return }
        readPending = false; start(.read, activity: .reading)
    }
    private func start(_ work: Work, activity: Activity) {
        guard worker == nil, active && !stopped else { return }
        timer?.cancel(); timer = nil
        self.activity = activity
        let client = client, token = generation
        worker = Task { [weak self] in
            do {
                let answer = try await Self.perform(work, client: client)
                if let self {
                    if self.active && self.generation == token && !Task.isCancelled { self.accept(answer) }
                    else if !work.isRead && !self.stopped {
                        self.result = SpotifySetupResult(title: "Request acknowledged after setup closed",
                                                         detail: "Refresh status on reopening. No browser was opened and no request was repeated.", uncertain: false)
                    }
                }
            } catch {
                if let self, !work.isRead && !self.stopped {
                    let uncertain = error as? SpotifySetupError == .uncertain
                    self.result = SpotifySetupResult(title: uncertain ? "Outcome uncertain — not repeated" : "Nothing sent",
                                                     detail: uncertain ? SpotifySetupError.uncertain.localizedDescription : Self.readProblem(error), uncertain: uncertain)
                }
            }
            guard let self else { return }
            self.worker = nil; self.activity = .idle
            if self.active && !self.stopped {
                if !work.isRead { self.readPending = true }
                self.startRead()
                if self.worker == nil { self.scheduleObservation() }
            }
        }
    }

    private func accept(_ answer: Answer) {
        switch answer {
        case .read(let auth, let sync):
            switch auth {
            case .value(let value):
                signIn = value; signInProblem = nil
                if !value.pending || consent?.redirectUri != value.redirectUri { consent = nil }
            case .failure(let why): signIn = nil; signInProblem = why
            }
            switch sync {
            case .value(let value): library = value; libraryProblem = nil
            case .failure(let why): library = nil; libraryProblem = why
            }
            observedAt = Date()
        case .begun(let value):
            consent = value
            result = SpotifySetupResult(title: "Consent link ready", detail: "Open Spotify explicitly, authorize read-only library access, then return here. The daemon keeps the credentials.", uncertain: false)
        case .completed:
            consent = nil
            result = SpotifySetupResult(title: "Daemon acknowledged Spotify sign-in", detail: "A library refresh is a separate operation. This does not prove that Sonos playback is configured.", uncertain: false)
        case .sync(let value, let sent):
            library = value
            result = SpotifySetupResult(title: sent ? "Library request acknowledged" : "Library refresh already running",
                                        detail: value.done + (sent ? " The status below reports progress; acceptance is not completion." : " No additional start request was sent."), uncertain: false)
        }
    }

    private func scheduleObservation() {
        guard active && !stopped && timer == nil else { return }
        let failed = signInProblem != nil || libraryProblem != nil
        guard failed || signIn?.pending == true || library?.running == true else { retry = timing.poll; return }
        let delay = failed ? retry : timing.poll, token = generation
        retry = failed ? min(timing.maximumRetry, retry * 2) : timing.poll
        timer = Task { [weak self] in
            do {
                try await Task.sleep(for: delay)
                guard let self, self.active && self.generation == token else { return }
                self.timer = nil; self.readPending = true; self.startRead()
            } catch { }
        }
    }

    private enum Work: Sendable {
        case read, begin(SpotifySignIn, Bool), complete(SpotifyConsent, String), sync
        var isRead: Bool { if case .read = self { true } else { false } }
    }
    private enum Read<T: Sendable>: Sendable { case value(T), failure(String) }
    private enum Answer: Sendable {
        case read(Read<SpotifySignIn>, Read<SpotifyLibrarySync>)
        case begun(SpotifyConsent), completed, sync(SpotifyLibrarySync, Bool)
    }
    nonisolated private static func read<T: Sendable>(_ operation: @Sendable () async throws -> T) async -> Read<T> {
        do { return .value(try await operation()) }
        catch { return .failure(readProblem(error)) }
    }
    nonisolated private static func perform(_ work: Work, client: DaemonClient) async throws -> Answer {
        try Task.checkCancellation()
        switch work {
        case .read:
            async let auth = read { try await client.spotifySignIn() }
            async let sync = read { try await client.spotifyLibrarySync() }
            let result = await Answer.read(auth, sync)
            try Task.checkCancellation()
            return result
        case .begin(let reviewed, let replace):
            let current = try await client.spotifySignIn()
            guard current == reviewed, !current.pending || replace else { throw SpotifySetupError.changedSignIn }
            try Task.checkCancellation()
            let consent = try await client.beginSpotifySignIn()
            guard consent.redirectUri == current.redirectUri else { throw SpotifySetupError.uncertain }
            return .begun(consent)
        case .complete(let consent, let callback):
            let current = try await client.spotifySignIn()
            guard current.pending, current.redirectUri == consent.redirectUri else { throw SpotifySetupError.changedSignIn }
            try Task.checkCancellation()
            _ = try await client.completeSpotifySignIn(callback, consent: consent)
            return .completed
        case .sync:
            let current = try await client.spotifyLibrarySync()
            try Task.checkCancellation()
            if current.running { return .sync(current, false) }
            return .sync(try await client.startSpotifyLibrarySync(), true)
        }
    }

    /// Auth errors may include addresses or codes. Preserve useful stable
    /// classifications, never arbitrary transport text or callback contents.
    nonisolated private static func readProblem(_ error: any Error) -> String {
        if let error = error as? SpotifySetupError { return error.localizedDescription }
        if error is CancellationError { return "The unsent request was cancelled." }
        if let problem = error as? APIProblem {
            switch problem.code {
            case "NOT_IMPLEMENTED": return "This daemon has no configured Spotify service. Configure its Spotify client ID and redirect URI on the daemon host."
            case "POLICY_DENIED": return "The daemon's policy denied this operation for the current caller."
            default: return "The daemon could not provide Spotify status. Check its configuration and access policy."
            }
        }
        return "Spotify status could not be read. Check the daemon connection and refresh."
    }

    deinit { worker?.cancel(); timer?.cancel() }
}
