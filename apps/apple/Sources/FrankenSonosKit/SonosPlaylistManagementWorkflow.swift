import Foundation

public struct SonosPlaylistManagementResult: Identifiable, Sendable {
    public enum Kind: Sendable { case confirmed, uncertain, notSent }
    public let id = UUID()
    public let command: SonosPlaylistMutation
    public let receipt: SonosPlaylistMutationReceipt?
    public let kind: Kind
    public let detail: String
    public var title: String {
        switch kind {
        case .confirmed: receipt?.done ?? "Saved playlist updated"
        case .uncertain: "Playlist outcome uncertain — not repeated"
        case .notSent: "Playlist change was not sent"
        }
    }
}

/// Owns native catalog reviews for the whole connection, not a disposable room
/// view. It shares the playback/scene command actor and retains original-scope
/// receipts while the user changes rooms, dismisses the sheet or backgrounds.
@MainActor public final class SonosPlaylistManagementWorkflow {
    public enum Phase: Sendable { case idle, preparing, confirming, checkingCatalog }
    public var onChange: (@MainActor () -> Void)?
    public var canBegin: @MainActor () -> Bool = { true }
    public var awaitBarrier: @MainActor () async -> Void = {}
    public var didChangeCatalog: (@MainActor () -> Void)?
    public private(set) var active = false
    public private(set) var isOpen = false
    public private(set) var operation: SonosPlaylistMutation.Operation?
    public private(set) var zone: Zone?
    public private(set) var playlist: SonosPlaylist?
    public private(set) var title = ""
    public private(set) var problem: String?
    public private(set) var review: SonosPlaylistMutationReview?
    public private(set) var result: SonosPlaylistManagementResult?
    public private(set) var recovery: SonosPlaylistCatalogRecovery?
    public private(set) var phase: Phase = .idle
    public private(set) var needsRefresh = false
    private let client: DaemonClient
    private let commands: CommandReviewSession
    private var task: Task<Void, Never>?
    private var cancellation: Task<Void, Never>?
    private var generation: UInt64 = 0
    private var stopped = false

    public var isEngaged: Bool { phase != .idle || task != nil || cancellation != nil || review != nil }
    public var canEdit: Bool { active && !stopped && isOpen && !isEngaged && canBegin() }
    public var canReview: Bool { canEdit && operation != nil && zone != nil && !needsRefresh }
    public var canCheckCatalog: Bool { canEdit && result != nil && result?.kind != .notSent }

    public init(client: DaemonClient, commands: CommandReviewSession) {
        self.client = client; self.commands = commands
    }
    private func publish() { onChange?() }
    public func resume() { guard !stopped else { return }; active = true; publish() }
    public func suspend() { active = false; close() }
    public func stop() { stopped = true; suspend() }

    @discardableResult public func openSave(in zone: Zone) -> Bool {
        open(.save, in: zone, playlist: nil)
    }
    @discardableResult public func openRename(_ playlist: SonosPlaylist, in zone: Zone) -> Bool {
        open(.rename, in: zone, playlist: playlist)
    }
    @discardableResult public func openRemove(_ playlist: SonosPlaylist, in zone: Zone) -> Bool {
        open(.remove, in: zone, playlist: playlist)
    }
    private func open(_ operation: SonosPlaylistMutation.Operation, in zone: Zone, playlist: SonosPlaylist?) -> Bool {
        guard active && !stopped && !isEngaged && canBegin() else { return false }
        if needsRefresh { return openResult() }
        do {
            _ = try zone.validated()
            if let playlist { _ = try playlist.validated() }
        } catch { problem = error.localizedDescription; publish(); return false }
        self.operation = operation; self.zone = zone; self.playlist = playlist
        title = operation == .rename ? playlist?.title ?? "" : ""
        problem = nil; review = nil; isOpen = true; publish()
        return true
    }

    /// Opening an earlier receipt never silently adopts the current room.
    @discardableResult public func openResult() -> Bool {
        guard active && !stopped && !isEngaged && canBegin(), result != nil else { return false }
        operation = nil; zone = nil; playlist = nil; title = ""; problem = nil
        isOpen = true; publish(); return true
    }
    public func editTitle(_ value: String) {
        guard canEdit && operation != .remove && value.utf8.count <= 16_384 else { return }
        title = value; problem = nil; publish()
    }
    public func close() { isOpen = false; cancel(); publish() }

    public func prepare() {
        guard canReview, let operation, let zone else { return }
        let command: SonosPlaylistMutation
        do {
            switch operation {
            case .save: command = try .save(title: title, in: zone)
            case .rename:
                guard let playlist else { throw ContractError.invalid("Choose an exact saved playlist.") }
                command = try .rename(playlist, title: title, in: zone)
            case .remove:
                guard let playlist else { throw ContractError.invalid("Choose an exact saved playlist.") }
                command = try .remove(playlist, in: zone)
            }
        } catch { problem = error.localizedDescription; publish(); return }
        generation &+= 1
        let version = generation, commands = commands, awaitBarrier = awaitBarrier
        phase = .preparing; problem = nil
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
                    self.problem = error.localizedDescription
                }
            }
            self?.finished()
        }
        publish()
    }

    public func confirm(_ id: UUID) {
        guard active && !stopped && task == nil && cancellation == nil,
              let reviewed = review, reviewed.id == id else { return }
        review = nil; result = nil; recovery = nil; problem = nil; phase = .confirming
        let commands = commands
        task = Task { [weak self] in
            do {
                let receipt = try await commands.execute(reviewed)
                self?.result = SonosPlaylistManagementResult(command: reviewed.command, receipt: receipt,
                    kind: .confirmed, detail: receipt.detail)
                self?.operation = nil
                self?.didChangeCatalog?()
            } catch {
                let uncertain: Bool
                if case ClientError.uncertain = error { uncertain = true } else { uncertain = false }
                if uncertain { self?.needsRefresh = true; self?.operation = nil }
                self?.result = SonosPlaylistManagementResult(command: reviewed.command, receipt: nil,
                    kind: uncertain ? .uncertain : .notSent,
                    detail: uncertain
                        ? "This catalog change may already have taken effect in \(reviewed.command.zone.household). It was not repeated. Check that original household's saved playlists before another catalog change.\n" + error.localizedDescription
                        : error.localizedDescription)
            }
            // A late acknowledgement stays with the original target, even
            // after close/suspend or a change to the selected household.
            self?.finished()
        }
        publish()
    }

    /// Only this explicit successful read permits another catalog mutation.
    /// A matching title or absent id never promotes the earlier result to ACK.
    public func checkCatalog() {
        guard canCheckCatalog, let observed = result else { return }
        generation &+= 1
        let version = generation, client = client
        phase = .checkingCatalog; problem = nil
        task = Task { [weak self] in
            do {
                let current = try await SonosPlaylistCatalogRecovery.read(observed, client: client)
                if let self, self.active && self.generation == version && self.result?.id == observed.id && !Task.isCancelled {
                    self.recovery = current; self.needsRefresh = false
                }
            } catch {
                if let self, self.active && self.generation == version && !Task.isCancelled {
                    self.problem = error.localizedDescription
                }
            }
            self?.finished()
        }
        publish()
    }

    public func cancel() {
        guard isEngaged else { return }
        generation &+= 1
        let version = generation, running = task, previous = cancellation, commands = commands
        review = nil; running?.cancel()
        cancellation = Task { [weak self] in
            await previous?.value
            await commands.cancel()
            await running?.value
            guard let self, self.generation == version else { return }
            self.cancellation = nil
            if self.task == nil { self.phase = .idle }
            self.publish()
        }
        publish()
    }

    public func dismissResult() {
        guard !isEngaged && !needsRefresh else { return }
        result = nil; recovery = nil; publish()
    }
    private func finished() { task = nil; phase = .idle; publish() }
}
