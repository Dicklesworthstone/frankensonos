import Foundation

public struct RemoteResult: Identifiable, Sendable {
    public enum Kind: String, Sendable { case confirmed, partial, uncertain, notSent }
    public let id = UUID()
    public let target: RoomID
    public let title: String
    public let detail: String
    public let kind: Kind
}

/// The native screen's state, independent of SwiftUI so its real command and
/// lifecycle paths run in package tests. Selection is a household-qualified
/// room anchor; it follows a coordinator change without retargeting a review.
@MainActor public final class RemoteModel {
    public enum Panel: String, CaseIterable, Sendable { case queue, favorites, dj, library }
    public enum ActionPhase: String, Sendable { case idle, preparing, confirming }

    /// A single UI adapter may observe state changes on the main actor. No
    /// UI framework or platform-specific observation runtime enters the kit.
    public var onChange: (@MainActor () -> Void)?

    public private(set) var house: LiveHouseView? { willSet { onChange?() } }
    public private(set) var selectedRoom: RoomID? { willSet { onChange?() } }
    public private(set) var active = false { willSet { onChange?() } }
    public private(set) var panel: Panel = .queue { willSet { onChange?() } }
    public private(set) var queue: QueuePage? { willSet { onChange?() } }
    public private(set) var queueOffset: UInt32 = 0 { willSet { onChange?() } }
    public private(set) var favorites: [Favorite] = [] { willSet { onChange?() } }
    public private(set) var dj: DJStatus? { willSet { onChange?() } }
    public private(set) var moods: [MoodCatalog.Mood] = [] { willSet { onChange?() } }
    public private(set) var hits: [LibraryHit] = [] { willSet { onChange?() } }
    public private(set) var query = "" { willSet { onChange?() } }
    public private(set) var panelLoading = false { willSet { onChange?() } }
    public private(set) var panelProblem: String? { willSet { onChange?() } }
    public private(set) var review: CommandReview? { willSet { onChange?() } }
    public private(set) var result: RemoteResult? { willSet { onChange?() } }
    public private(set) var actionPhase: ActionPhase = .idle { willSet { onChange?() } }

    private let client: DaemonClient
    private let live: LiveHouse
    private let commands: CommandReviewSession
    private var observer: Task<Void, Never>?
    private var lifecycle: Task<Void, Never>?
    private var actionTask: Task<Void, Never>?
    private var cancellation: Task<Void, Never>?
    private var panelTask: Task<Void, Never>?
    private var epoch: UInt64 = 0
    private var panelEpoch: UInt64 = 0
    private var panelPending = false
    private var stopped = false
    private var choseInitialRoom = false

    public var selectedZone: Zone? {
        guard let selectedRoom else { return nil }
        return house?.snapshot?.zones.first { $0.rooms.contains(selectedRoom) }
    }
    public var selectedState: ZoneState? {
        guard let zone = selectedZone else { return nil }
        return house?.snapshot?.states[zone.id]
    }
    public var canPrepare: Bool { active && !stopped && selectedZone != nil && actionPhase == .idle && review == nil }

    public init(client: DaemonClient) {
        self.client = client
        live = LiveHouse(client: client)
        commands = CommandReviewSession(client: client)
    }

    public func resume() {
        guard !active && !stopped else { return }
        active = true
        if observer == nil {
            let stream = live.updates
            observer = Task { [weak self] in
                for await view in stream {
                    guard !Task.isCancelled else { break }
                    self?.receive(view)
                }
            }
        }
        let previous = lifecycle, live = live
        lifecycle = Task { await previous?.value; await live.resume() }
        refreshPanel()
    }

    public func suspend() {
        guard !stopped else { return }
        active = false
        cancelReview()
        invalidatePanel()
        let previous = lifecycle, live = live
        lifecycle = Task { await previous?.value; await live.suspend() }
    }

    public func stop() {
        guard !stopped else { return }
        suspend(); stopped = true
        observer?.cancel(); observer = nil
        let previous = lifecycle, live = live
        lifecycle = Task { await previous?.value; await live.stop() }
    }

    public func select(_ room: RoomID) {
        guard house?.snapshot?.zones.contains(where: { $0.rooms.contains(room) }) == true else { return }
        guard selectedRoom != room else { return }
        cancelReview()
        selectedRoom = room; choseInitialRoom = true
        queueOffset = 0
        invalidatePanel(); refreshPanel()
    }

    public func show(_ panel: Panel) {
        guard self.panel != panel else { return }
        self.panel = panel
        invalidatePanel(); refreshPanel()
    }

    public func refresh() {
        guard active else { return }
        let live = live
        Task { await live.refresh() }
        refreshPanel()
    }

    public func search(_ text: String) {
        guard active else { return }
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard trimmed.utf8.count <= 512 else { panelProblem = "Keep the search under 512 bytes."; return }
        panel = .library; query = trimmed
        invalidatePanel(); refreshPanel()
    }

    public func queuePage(offset: UInt32) {
        guard active else { return }
        panel = .queue; queueOffset = offset
        invalidatePanel(); refreshPanel()
    }

    /// Every visible command enters this path; changing a slider does not send
    /// anything. A new review is based on fresh reads, not the cached display.
    public func prepare(_ command: NativeCommand, in expectedZone: Zone? = nil) {
        guard canPrepare, let zone = selectedZone, expectedZone.map({ $0.sameGroup(as: zone) }) != false else { return }
        let target = RoomID(household: zone.household, room: command.room ?? zone.coordinatorRoom)
        actionPhase = .preparing
        let token = epoch, barrier = cancellation, commands = commands
        actionTask = Task { [weak self] in
            defer { self?.actionFinished() }
            await barrier?.value
            do {
                try Task.checkCancellation()
                let review = try await commands.prepare(command, in: zone)
                guard let self, self.epoch == token && self.active && !Task.isCancelled else { return }
                self.review = review
            } catch {
                if let self, self.epoch == token && !Task.isCancelled {
                    self.result = RemoteResult(target: target, title: "Nothing sent", detail: error.localizedDescription, kind: .notSent)
                }
            }
        }
    }

    public func confirm(_ id: UUID) {
        guard active, actionPhase == .idle, let reviewed = review, reviewed.id == id else { return }
        review = nil; actionPhase = .confirming
        let target = RoomID(household: reviewed.zone.household, room: reviewed.command.room ?? reviewed.zone.coordinatorRoom)
        let commands = commands
        actionTask = Task { [weak self] in
            do {
                let receipt = try await commands.execute(reviewed)
                self?.result = RemoteResult(target: target, title: receipt.title, detail: receipt.detail,
                                            kind: receipt.partial ? .partial : .confirmed)
            } catch {
                let uncertain: Bool
                if case ClientError.uncertain = error { uncertain = true } else { uncertain = false }
                self?.result = RemoteResult(target: target,
                                            title: uncertain ? "Outcome uncertain — not repeated" : "Nothing sent",
                                            detail: error.localizedDescription, kind: uncertain ? .uncertain : .notSent)
            }
            // A late acknowledgement stays attributed to its original room,
            // even after selection or lifecycle changes. Refresh is not an ACK.
            self?.actionFinished()
            self?.refresh()
        }
    }

    public func cancelReview() {
        epoch &+= 1; review = nil
        actionTask?.cancel()
        let previous = cancellation, commands = commands
        cancellation = Task { await previous?.value; await commands.cancel() }
        // The busy slot stays occupied until the cancelled I/O drains.
    }

    public func dismissResult() { result = nil }

    private func actionFinished() { actionTask = nil; actionPhase = .idle }

    private func receive(_ value: LiveHouseView) {
        guard !stopped else { return }
        let oldZone = selectedZone
        let oldState = selectedState
        let wasFresh = house?.isFresh == true
        house = value
        if !choseInitialRoom, let first = value.snapshot?.zones.first {
            choseInitialRoom = true; selectedRoom = first.id
        }
        let newZone = selectedZone
        let layoutChanged = oldZone.map { old in newZone.map { !old.sameGroup(as: $0) } ?? true } ?? (newZone != nil)
        if layoutChanged {
            // A grouping command can emit its topology event before its HTTP
            // acknowledgement. Do not cancel that very write on its own event.
            // Explicit selection changes and suspension still cancel requests.
            if actionPhase != .confirming { cancelReview() }
            queueOffset = 0
            invalidatePanel(); refreshPanel()
        } else if active && value.isFresh && (!wasFresh || oldState.map { old in selectedState.map { !old.samePlayback(as: $0) } ?? true } == true) {
            if panel == .queue || panel == .dj { refreshPanel() }
        }
    }

    private func invalidatePanel() {
        panelEpoch &+= 1
        panelTask?.cancel(); panelPending = false
        queue = nil; favorites = []; dj = nil; moods = []; hits = []
        panelProblem = nil
        // Keep a cancelled request's slot until it drains, including a
        // transport that ignores cancellation. Never publish its stale data.
        if panelTask == nil { panelLoading = false }
    }

    public func refreshPanel() {
        guard active && !stopped && selectedZone != nil else { return }
        panelPending = true
        startPanelRead()
    }

    private func startPanelRead() {
        guard active && !stopped && panelPending && panelTask == nil, let zone = selectedZone else { return }
        panelPending = false; panelLoading = true; panelProblem = nil
        let token = panelEpoch, panel = panel, offset = queueOffset, query = query, client = client
        panelTask = Task { [weak self] in
            do {
                let answer: PanelAnswer
                switch panel {
                case .queue: answer = .queue(try await client.queue(zone, offset: offset))
                case .favorites: answer = .favorites(try await client.favorites(zone))
                case .dj:
                    let status = try await client.dj(zone)
                    do {
                        let moods = try await client.get(MoodCatalog.self, ["dj", "moods"], query: [URLQueryItem(name: "zone", value: zone.id.address)])
                        answer = .dj(status, moods.moods, nil)
                    } catch {
                        try Task.checkCancellation()
                        answer = .dj(status, [], "Mood catalog unavailable: " + error.localizedDescription)
                    }
                case .library: answer = .library(query.isEmpty ? [] : try await client.search(query, in: zone))
                }
                try Task.checkCancellation()
                if let self, self.active && self.panelEpoch == token {
                    switch answer {
                    case .queue(let value): self.queue = value
                    case .favorites(let value): self.favorites = value
                    case .dj(let value, let moods, let problem): self.dj = value; self.moods = moods; self.panelProblem = problem
                    case .library(let value): self.hits = value
                    }
                }
            } catch {
                if let self, self.active && self.panelEpoch == token && !Task.isCancelled {
                    self.queue = nil; self.favorites = []; self.dj = nil; self.moods = []; self.hits = []
                    self.panelProblem = error.localizedDescription
                }
            }
            self?.panelTask = nil; self?.panelLoading = false
            self?.startPanelRead()
        }
    }

    deinit { observer?.cancel(); panelTask?.cancel(); actionTask?.cancel() }
}

private enum PanelAnswer: Sendable {
    case queue(QueuePage), favorites([Favorite]), dj(DJStatus, [MoodCatalog.Mood], String?), library([LibraryHit])
}
