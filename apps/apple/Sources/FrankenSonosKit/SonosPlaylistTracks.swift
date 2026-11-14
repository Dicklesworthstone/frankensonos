import Foundation

/// One occurrence in a locally ordered selection. Repeats have distinct ids,
/// while their original saved-playlist positions and observed pages stay intact.
public struct SonosPlaylistTrackChoice: Equatable, Identifiable, Sendable {
    public let id: UUID
    public let item: SonosPlaylistItem
    public let request: SonosPlaylistContentsRequest

    fileprivate init(item: SonosPlaylistItem, request: SonosPlaylistContentsRequest) {
        id = UUID(); self.item = item; self.request = request
    }
}

/// At most ten explicitly observed entries, in the listener's chosen order.
/// Paging keeps the selection only within the same household, playlist and revision.
public struct SonosPlaylistTracksDraft: Equatable, Sendable {
    public static let maximumCount = 10
    public private(set) var id = UUID()
    public private(set) var zone: Zone?
    public private(set) var playlist: SonosPlaylist?
    public private(set) var revision: String?
    public private(set) var total: UInt32 = 0
    public private(set) var choices: [SonosPlaylistTrackChoice] = []
    public var isEmpty: Bool { choices.isEmpty }

    public init() {}
    public mutating func clear() { self = Self() }

    public static func canSelect(_ item: SonosPlaylistItem) -> Bool {
        item.position > 0 && item.uri.map { !$0.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty } == true
    }

    static func sameOccurrence(_ lhs: SonosPlaylistItem, _ rhs: SonosPlaylistItem) -> Bool {
        lhs.position == rhs.position && lhs.durationSecs == rhs.durationSecs
            && PlaylistContentsRequest.exact(lhs.title, rhs.title)
            && PlaylistContentsRequest.exact(lhs.creator, rhs.creator)
            && PlaylistContentsRequest.exact(lhs.album, rhs.album)
            && PlaylistContentsRequest.exact(lhs.uri, rhs.uri)
    }

    func matches(_ page: SonosPlaylistContents, request: SonosPlaylistContentsRequest) -> Bool {
        zone?.sameGroup(as: request.zone) == true
            && playlist?.sameSelection(as: page.playlist) == true
            && PlaylistContentsRequest.exact(revision, page.revision) && total == page.total
    }

    public mutating func observe(_ page: SonosPlaylistContents, request: SonosPlaylistContentsRequest) throws {
        do {
            _ = try page.validated(for: request)
            if !matches(page, request: request) { clear() }
            for choice in choices {
                guard let item = page.items.first(where: { $0.position == choice.item.position }) else { continue }
                guard Self.sameOccurrence(item, choice.item) else {
                    throw ContractError.invalid("A selected saved-playlist occurrence changed. Select the tracks again.")
                }
            }
            zone = request.zone; playlist = page.playlist; revision = page.revision; total = page.total
        } catch {
            clear()
            throw error
        }
    }

    public mutating func append(_ item: SonosPlaylistItem, from page: SonosPlaylistContents,
                                request: SonosPlaylistContentsRequest) throws {
        try observe(page, request: request)
        guard Self.canSelect(item), page.items.contains(where: { Self.sameOccurrence($0, item) }) else {
            throw ContractError.invalid("Choose a playable occurrence from the current saved-playlist page.")
        }
        guard choices.count < Self.maximumCount else {
            throw ContractError.invalid("Choose at most ten saved-playlist occurrences for one action.")
        }
        let observed = try SonosPlaylistContentsRequest(playlist: page.playlist, in: request.zone,
            offset: page.offset, expectedRevision: page.revision)
        choices.append(SonosPlaylistTrackChoice(item: item, request: observed)); id = UUID()
    }

    public mutating func repeatChoice(_ choiceID: UUID) throws {
        guard let choice = choices.first(where: { $0.id == choiceID }), choices.count < Self.maximumCount else {
            throw ContractError.invalid("Choose a current selected entry, with room for at most ten occurrences.")
        }
        choices.append(SonosPlaylistTrackChoice(item: choice.item, request: choice.request)); id = UUID()
    }

    public mutating func remove(_ choiceID: UUID) throws {
        guard let index = choices.firstIndex(where: { $0.id == choiceID }) else {
            throw ContractError.invalid("This selected occurrence is no longer current.")
        }
        choices.remove(at: index); id = UUID()
    }

    public mutating func move(_ choiceID: UUID, by step: Int) throws {
        guard [-1, 1].contains(step), let index = choices.firstIndex(where: { $0.id == choiceID }),
              choices.indices.contains(index + step) else {
            throw ContractError.invalid("Move a selected occurrence one place within the selection.")
        }
        choices.swapAt(index, index + step); id = UUID()
    }

    public func isCurrent(_ token: UUID, page: SonosPlaylistContents,
                          request: SonosPlaylistContentsRequest) -> Bool {
        id == token && !isEmpty && matches(page, request: request)
    }
}

/// Exact native occurrences, with metadata supplied exclusively by the daemon.
/// Play replaces the queue in NORMAL order; next/append do not start playback.
public struct SonosPlaylistTracksSelection: Equatable, Sendable {
    public let zone: Zone
    public let playlist: SonosPlaylist
    public let revision: String
    public let total: UInt32
    public let choices: [SonosPlaylistTrackChoice]
    public let action: SonosPlaylistAction
    public private(set) var expectedQueueRevision: String?

    public init(_ draft: SonosPlaylistTracksDraft, action: SonosPlaylistAction) throws {
        guard let zone = draft.zone, let playlist = draft.playlist, let revision = draft.revision,
              SonosPlaylistContentsRequest.validRevision(revision),
              (1...SonosPlaylistTracksDraft.maximumCount).contains(draft.choices.count),
              draft.choices.allSatisfy({ choice in
                  SonosPlaylistTracksDraft.canSelect(choice.item) && choice.item.position <= draft.total
                      && choice.request.zone.sameGroup(as: zone)
                      && choice.request.playlist.sameSelection(as: playlist)
                      && PlaylistContentsRequest.exact(choice.request.expectedRevision, revision)
              }) else {
            throw ContractError.invalid("Choose one to ten current saved-playlist occurrences before reviewing playback.")
        }
        self.zone = try zone.validated(); self.playlist = try playlist.validated()
        self.revision = revision; total = draft.total; choices = draft.choices; self.action = action
    }

    public var positions: [UInt32] { choices.map(\.item.position) }
    public var title: String { "\(action.title): \(choices.count) selected \(choices.count == 1 ? "track" : "tracks")" }
    public var explanation: String {
        let order = choices.enumerated().map { index, choice in
            "\(index + 1). \(choice.item.title.isEmpty ? "Untitled entry" : choice.item.title) · saved position \(choice.item.position)"
        }.joined(separator: "\n")
        let effect = action == .play
            ? "Replaces this group's queue and starts these entries in the selected order with shuffle and repeat off. Playback caps apply. Undo depends on the daemon retaining and verifying a complete bounded backup; inspect the result and action history."
            : action == .next
                ? "Inserts this selection after the current queue entry, or at the end when no queue entry is selected. Playback is not started or resumed."
                : "Appends this selection in order without starting or resuming playback."
        return "\(playlist.title) · \(zone.title) · \(zone.household)\n\(order)\n\n\(effect) Repeated positions are intentional repeats. The saved contents and destination queue are checked again before one command is sent."
    }

    func bound(toQueueRevision value: String) throws -> Self {
        guard !value.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty, value.utf8.count <= 1_024,
              !value.unicodeScalars.contains(where: CharacterSet.controlCharacters.contains) else {
            throw ContractError.invalid("Read the destination queue before reviewing these saved tracks.")
        }
        var copy = self; copy.expectedQueueRevision = value; return copy
    }

    func fields(in current: Zone) throws -> [String: JSONValue] {
        _ = try current.validated()
        guard zone.sameGroup(as: current) else {
            throw ContractError.invalid("The selected tracks belong to a different household or group. Review them again.")
        }
        var fields: [String: JSONValue] = [
            "zone": .string(zone.id.address), "playlist": .string(playlist.id),
            "positions": .array(positions.map { .integer(Int64($0)) }),
            "expected_revision": .string(revision), "mode": .string(action.rawValue)
        ]
        if let expectedQueueRevision { fields["expected_queue_revision"] = .string(expectedQueueRevision) }
        return fields
    }

    func validate(using read: (SonosPlaylistContentsRequest) async throws -> SonosPlaylistContents) async throws {
        var offsets = Set<UInt32>()
        for choice in choices where offsets.insert(choice.request.offset).inserted {
            try Task.checkCancellation()
            let page = try await read(choice.request).validated(for: choice.request)
            try Task.checkCancellation()
            guard page.total == total else {
                throw ContractError.invalid("The saved playlist length changed. Select the tracks again.")
            }
            for expected in choices where expected.request.offset == choice.request.offset {
                guard page.items.contains(where: { SonosPlaylistTracksDraft.sameOccurrence($0, expected.item) }) else {
                    throw ContractError.invalid("A selected saved-playlist occurrence changed. Nothing was sent.")
                }
            }
        }
    }

    func receipt(_ data: Data, in current: Zone?) throws -> CommandReceipt {
        guard let current else { throw ClientError.unreadable }
        _ = try fields(in: current)
        struct Receipt: Decodable {
            let done: String
            let zone: String
            let playlist: String
            let positions: [UInt32]
            let titles: [String]
            let mode: String
            let changed: Bool
            let notes: [Note]
            let first: UInt32?
            let count: UInt32?
            let playingQueue: Bool?
        }
        let report = try DaemonClient.decode(Receipt.self, from: data)
        guard !report.done.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              report.zone.utf8.elementsEqual(zone.coordinatorRoom.utf8),
              report.playlist.utf8.elementsEqual(playlist.id.utf8), report.positions == positions,
              report.mode == action.rawValue, report.titles.count == choices.count,
              zip(report.titles, choices).allSatisfy({ $0.0.utf8.elementsEqual($0.1.item.title.utf8) }),
              report.notes.allSatisfy({ !$0.code.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
                  && !$0.detail.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty }) else {
            throw ClientError.unreadable
        }
        let detail: String
        if action == .play {
            guard report.first == nil && report.count == nil && report.playingQueue == nil else { throw ClientError.unreadable }
            detail = report.changed
                ? "Played \(choices.count) selected saved-playlist occurrences in the supplied order."
                : "No playback change was reported for the selected saved-playlist occurrences."
        } else {
            guard report.changed, let first = report.first, first > 0, report.count == UInt32(choices.count),
                  let playingQueue = report.playingQueue,
                  UInt64(first) + UInt64(choices.count) - 1 <= UInt64(UInt32.max) else { throw ClientError.unreadable }
            detail = "Added \(choices.count) selected occurrences at queue position \(first). " +
                (playingQueue ? "Playback was not started or restarted." : "The group's current source was left unchanged.")
        }
        return CommandReceipt(title: report.done,
            detail: ([detail] + report.notes.map(\.detail)).joined(separator: "\n"),
            partial: report.notes.contains(where: { $0.code == "SKIPPED" }))
    }
}
