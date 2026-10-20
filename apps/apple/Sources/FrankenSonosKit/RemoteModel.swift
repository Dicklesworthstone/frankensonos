import Foundation

public struct RemoteResult: Identifiable, Sendable {
    public enum Kind: String, Sendable { case confirmed, partial, uncertain, notSent }
    public let id = UUID()
    public let target: RoomID
    /// A scene is house-wide; `target` only identifies the initiating selection.
    public let scene: String?
    public let title: String
    public let detail: String
    public let kind: Kind
}

/// The native screen's state, independent of SwiftUI so its real command and
/// lifecycle paths run in package tests. Selection is a household-qualified
/// room anchor; it follows a coordinator change without retargeting a review.
@MainActor public final class RemoteModel {
    public enum Panel: String, CaseIterable, Sendable { case queue, favorites, dj, library, scenes, sleep, schedules }
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
    public private(set) var scenes: [Scene] = [] { willSet { onChange?() } }
    public private(set) var sleepTimer: SleepTimer? { willSet { onChange?() } }
    /// Nil means unread/failed, not a successful report of no timer.
    public private(set) var sleepObservedAt: Date? { willSet { onChange?() } }
    public private(set) var query = "" { willSet { onChange?() } }
    public private(set) var panelLoading = false { willSet { onChange?() } }
    public private(set) var panelProblem: String? { willSet { onChange?() } }
    public private(set) var review: CommandReview? { willSet { onChange?() } }
    public private(set) var result: RemoteResult? { willSet { onChange?() } }
    public private(set) var actionPhase: ActionPhase = .idle { willSet { onChange?() } }

    public private(set) var schedules: [SavedSchedule] = [] { willSet { onChange?() } }
    public private(set) var schedulesObservedAt: Date? { willSet { onChange?() } }
    public private(set) var scheduleScenes: [Scene] = [] { willSet { onChange?() } }
    public private(set) var scheduleScenesProblem: String? { willSet { onChange?() } }
    public private(set) var scheduleForm = ScheduleForm() { willSet { onChange?() } }
    public private(set) var scheduleFormProblem: String? { willSet { onChange?() } }
    public private(set) var scheduleReview: ScheduleReview? { willSet { onChange?() } }
    public private(set) var scheduleResult: RemoteScheduleResult? { willSet { onChange?() } }
    private var scheduleAction = false
    public private(set) var undoReview: UndoReview? { willSet { onChange?() } }
    public private(set) var undoResult: RemoteUndoResult? { willSet { onChange?() } }
    private var undoAction = false

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
    private var timelineFrozenAt: Date?

    public var selectedZone: Zone? {
        guard let selectedRoom else { return nil }
        return house?.snapshot?.zones.first { $0.rooms.contains(selectedRoom) }
    }
    public var selectedState: ZoneState? {
        guard let zone = selectedZone else { return nil }
        return house?.snapshot?.states[zone.id]
    }
    public var selectedTimeline: PlaybackTimeline? {
        guard let zone = selectedZone, let state = selectedState,
              let observedAt = house?.snapshot?.stateObservedAt[zone.id] else { return nil }
        return PlaybackTimeline(track: state.track, isPlaying: state.isPlaying, observedAt: observedAt,
                                isFresh: active && house?.isFresh == true,
                                allowsSeeking: ["playing", "paused"].contains(state.transportState),
                                frozenAt: timelineFrozenAt)
    }
    public var artworkURL: URL? {
        guard let zone = selectedZone else { return nil }
        return client.artworkURL(in: zone, track: selectedState?.track)
    }
    public var canPrepare: Bool { canSchedule && selectedZone != nil }
    public var canSchedule: Bool { active && !stopped && actionPhase == .idle && review == nil && scheduleReview == nil && undoReview == nil }
    public var canManageSchedules: Bool { canSchedule && panel == .schedules && schedulesObservedAt != nil && !panelLoading }
    public var undoInProgress: Bool { undoAction }

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
        if timelineFrozenAt == nil { timelineFrozenAt = Date() }
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
        if self.panel == .schedules { cancelReview() }
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
                    self.result = RemoteResult(target: target, scene: command.savedScene?.name, title: "Nothing sent", detail: error.localizedDescription, kind: .notSent)
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
                self?.result = RemoteResult(target: target, scene: reviewed.command.savedScene?.name, title: receipt.title, detail: receipt.detail,
                                            kind: receipt.partial ? .partial : .confirmed)
            } catch {
                let uncertain: Bool
                if case ClientError.uncertain = error { uncertain = true } else { uncertain = false }
                self?.result = RemoteResult(target: target, scene: reviewed.command.savedScene?.name,
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
        epoch &+= 1; review = nil; scheduleReview = nil; undoReview = nil
        actionTask?.cancel()
        let previous = cancellation, commands = commands
        cancellation = Task { await previous?.value; await commands.cancel() }
        // The busy slot stays occupied until the cancelled I/O drains.
    }

    public func dismissResult() { result = nil }

    private func actionFinished() { actionTask = nil; scheduleAction = false; undoAction = false; actionPhase = .idle }

    private func receive(_ value: LiveHouseView) {
        guard !stopped else { return }
        let oldZone = selectedZone
        let oldState = selectedState
        let wasFresh = house?.isFresh == true
        if active && value.isFresh { timelineFrozenAt = nil }
        else if timelineFrozenAt == nil { timelineFrozenAt = Date() }
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
            if actionPhase != .confirming && !scheduleAction && scheduleReview == nil && !undoAction && undoReview == nil { cancelReview() }
            queueOffset = 0
            invalidatePanel(); refreshPanel()
        } else if active && value.isFresh && (!wasFresh || oldState.map { old in selectedState.map { !old.samePlayback(as: $0) } ?? true } == true) {
            if panel == .queue || panel == .dj || panel == .sleep { refreshPanel() }
        }
    }

    private func invalidatePanel() {
        panelEpoch &+= 1
        panelTask?.cancel(); panelPending = false
        queue = nil; favorites = []; dj = nil; moods = []; hits = []; scenes = []; sleepTimer = nil; sleepObservedAt = nil
        schedules = []; schedulesObservedAt = nil; scheduleScenes = []; scheduleScenesProblem = nil
        panelProblem = nil
        // Keep a cancelled request's slot until it drains, including a
        // transport that ignores cancellation. Never publish its stale data.
        if panelTask == nil { panelLoading = false }
    }

    public func refreshPanel() {
        guard active && !stopped && (panel == .schedules || selectedZone != nil) else { return }
        panelPending = true
        startPanelRead()
    }

    private func startPanelRead() {
        guard active && !stopped && panelPending && panelTask == nil,
              panel == .schedules || selectedZone != nil else { return }
        let zone = selectedZone
        panelPending = false; panelLoading = true; panelProblem = nil
        let token = panelEpoch, panel = panel, offset = queueOffset, query = query, client = client
        panelTask = Task { [weak self] in
            do {
                let answer: PanelAnswer
                if panel == .schedules {
                    let values = try await client.schedules()
                    do {
                        answer = .schedules(values, try await client.scenes(), nil)
                    } catch {
                        try Task.checkCancellation()
                        answer = .schedules(values, [], "Scene catalog unavailable: " + error.localizedDescription)
                    }
                } else if let zone {
                    switch panel {
                    case .schedules: throw ContractError.invalid("The schedule read was not selected.")
                    case .queue: answer = .queue(try await client.queue(zone, offset: offset))
                    case .favorites: answer = .favorites(try await client.favorites(zone))
                    case .scenes: answer = .scenes(try await client.scenes())
                    case .sleep: answer = .sleep(try await client.sleepTimer(zone))
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
                } else { throw ContractError.invalid("Select a room for this panel.") }
                try Task.checkCancellation()
                if let self, self.active && self.panelEpoch == token {
                    switch answer {
                    case .schedules(let values, let scenes, let problem):
                        self.schedules = values; self.schedulesObservedAt = Date()
                        self.scheduleScenes = scenes; self.scheduleScenesProblem = problem
                    case .queue(let value): self.queue = value
                    case .favorites(let value): self.favorites = value
                    case .scenes(let value): self.scenes = value
                    case .sleep(let value): self.sleepTimer = value; self.sleepObservedAt = Date()
                    case .dj(let value, let moods, let problem): self.dj = value; self.moods = moods; self.panelProblem = problem
                    case .library(let value): self.hits = value
                    }
                }
            } catch {
                if let self, self.active && self.panelEpoch == token && !Task.isCancelled {
                    self.queue = nil; self.favorites = []; self.dj = nil; self.moods = []; self.hits = []; self.scenes = []; self.sleepTimer = nil; self.sleepObservedAt = nil
                    self.schedules = []; self.schedulesObservedAt = nil
                    self.scheduleScenes = []; self.scheduleScenesProblem = nil
                    self.panelProblem = error.localizedDescription
                }
            }
            self?.panelTask = nil; self?.panelLoading = false
            self?.startPanelRead()
        }
    }

    /// Edits only the unsent form. Selection and refresh never rewrite it.
    public func updateScheduleForm(_ value: ScheduleForm) {
        guard active && !stopped && actionPhase == .idle && review == nil && undoReview == nil else { return }
        if scheduleReview != nil { cancelReview() }
        scheduleForm = value; scheduleFormProblem = nil
    }

    public func prepareScheduleDraft() {
        guard canManageSchedules else { return }
        do {
            let draft = try scheduleForm.draft(zones: house?.snapshot?.zones ?? [], scenes: scheduleScenes)
            scheduleFormProblem = nil
            prepareSchedule(.add(draft))
        } catch { scheduleFormProblem = error.localizedDescription }
    }

    public func prepareSchedule(_ command: ScheduleCommand) {
        guard canManageSchedules else { return }
        if case .change(let expected, _) = command {
            guard schedules.contains(where: { expected.sameDefinition(as: $0) }) else {
                scheduleFormProblem = "This schedule is not in the current list. Refresh schedules."
                return
            }
        }
        actionPhase = .preparing; scheduleAction = true
        let token = epoch, barrier = cancellation, commands = commands
        actionTask = Task { [weak self] in
            defer { self?.actionFinished() }
            await barrier?.value
            do {
                try Task.checkCancellation()
                let observed = try await commands.prepare(command)
                guard let self, self.active && self.epoch == token && !Task.isCancelled else { return }
                self.scheduleReview = observed
            } catch {
                if let self, self.epoch == token && !Task.isCancelled {
                    self.scheduleResult = RemoteScheduleResult(command: command, receipt: nil, title: "Nothing sent",
                        detail: error.localizedDescription, kind: .notSent)
                }
            }
        }
    }

    public func confirmSchedule(_ id: UUID) {
        guard active, actionPhase == .idle, let reviewed = scheduleReview, reviewed.id == id else { return }
        scheduleReview = nil; actionPhase = .confirming; scheduleAction = true
        let commands = commands
        actionTask = Task { [weak self] in
            do {
                let receipt = try await commands.execute(reviewed)
                self?.scheduleResult = RemoteScheduleResult(command: reviewed.command, receipt: receipt,
                    title: receipt.title, detail: receipt.detail, kind: .confirmed)
            } catch {
                let uncertain: Bool
                if case ClientError.uncertain = error { uncertain = true } else { uncertain = false }
                self?.scheduleResult = RemoteScheduleResult(command: reviewed.command, receipt: nil,
                    title: uncertain ? "Schedule outcome uncertain — not repeated" : "Nothing sent",
                    detail: error.localizedDescription, kind: uncertain ? .uncertain : .notSent)
            }
            self?.actionFinished()
            self?.refreshPanel()
        }
    }

    public func dismissScheduleResult() { scheduleResult = nil }

    /// Ask the daemon for this caller's current recovery candidate. Activity
    /// filters and the selected room never choose or narrow the restore target.
    public func prepareUndo() {
        guard canSchedule else { return }
        actionPhase = .preparing; undoAction = true
        undoResult = nil
        let token = epoch, barrier = cancellation, commands = commands
        actionTask = Task { [weak self] in
            defer { self?.actionFinished() }
            await barrier?.value
            do {
                try Task.checkCancellation()
                let observed = try await commands.prepareUndo()
                guard let self, self.active && self.epoch == token && !Task.isCancelled else { return }
                self.undoReview = observed
            } catch {
                if let self, self.epoch == token && !Task.isCancelled {
                    self.undoResult = RemoteUndoResult(action: nil, receipt: nil, title: "Nothing sent",
                        detail: error.localizedDescription, kind: .notSent)
                }
            }
        }
    }

    public func confirmUndo(_ id: UUID) {
        guard active, actionPhase == .idle, let reviewed = undoReview, reviewed.id == id else { return }
        undoReview = nil; actionPhase = .confirming; undoAction = true
        let commands = commands
        actionTask = Task { [weak self] in
            do {
                let receipt = try await commands.execute(reviewed)
                self?.undoResult = RemoteUndoResult(action: reviewed.action, receipt: receipt,
                    title: receipt.failures.isEmpty ? "Undo completed" : "Undo partially completed",
                    detail: receipt.summary, kind: receipt.failures.isEmpty ? .confirmed : .partial)
            } catch {
                let uncertain: Bool
                if case ClientError.uncertain = error { uncertain = true } else { uncertain = false }
                self?.undoResult = RemoteUndoResult(action: reviewed.action, receipt: nil,
                    title: uncertain ? "Undo outcome uncertain — not repeated" : "Nothing sent",
                    detail: error.localizedDescription, kind: uncertain ? .uncertain : .notSent)
            }
            // Keep a late receipt on its original action after a room switch,
            // dismissal or suspension. A later refresh is never its substitute.
            self?.actionFinished()
            self?.refresh()
        }
    }

    public func cancelUndo() {
        guard undoReview != nil || undoAction else { return }
        cancelReview()
    }

    public func dismissUndoResult() { undoResult = nil }

    deinit { observer?.cancel(); panelTask?.cancel(); actionTask?.cancel() }
}

private enum PanelAnswer: Sendable {
    case schedules([SavedSchedule], [Scene], String?)
    case queue(QueuePage), favorites([Favorite]), dj(DJStatus, [MoodCatalog.Mood], String?), library([LibraryHit]), scenes([Scene]), sleep(SleepTimer?)
}
