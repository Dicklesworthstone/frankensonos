import Foundation

public enum QueueMoveDestination: String, CaseIterable, Sendable {
    case earlier, later, afterCurrent, start, end

    public var title: String {
        switch self {
        case .earlier: "Move earlier"
        case .later: "Move later"
        case .afterCurrent: "After current entry"
        case .start: "Move to start"
        case .end: "Move to end"
        }
    }
}

/// One to 100 consecutive occurrences in an observed queue. Both wire
/// positions refer to the original queue; never recalculate against a new one.
public struct QueueMove: Equatable, Sendable {
    public static let maximumCount = 100
    public let items: [QueueItem]
    /// The first occurrence, retained for existing single-entry callers.
    public var item: QueueItem { items[0] }
    public var count: UInt32 { UInt32(items.count) }
    public let destination: QueueMoveDestination
    public let revision: String
    public let total: UInt32
    public let playing: UInt32?
    public let insertBefore: UInt32
    public let resultingPosition: UInt32
    let sourceURIs: [String]
    var sourceURI: String { sourceURIs[0] }

    public init(_ item: QueueItem, in page: QueuePage, to destination: QueueMoveDestination) throws {
        try self.init([item], in: page, to: destination)
    }

    public init(_ items: [QueueItem], in page: QueuePage, to destination: QueueMoveDestination) throws {
        guard !items.isEmpty, items.count <= Self.maximumCount,
              let first = page.items.first, first.position > 0,
              let revision = page.revision, Self.validRevision(revision) else {
            throw ContractError.invalid("Refresh this queue before arranging its entries.")
        }
        _ = try page.validated(offset: first.position - 1, limit: UInt32(Self.maximumCount))
        let position = UInt64(items[0].position), count = UInt64(items.count), total = UInt64(page.total)
        guard position > 0, position + count - 1 <= total,
              items.enumerated().allSatisfy({ index, item in
                  UInt64(item.position) == position + UInt64(index) && Self.hasIdentity(item) &&
                  page.items.contains(where: { Self.sameOccurrence($0, item) })
              }) else {
            throw ContractError.invalid("Select one consecutive block of known queue entries, in its original order.")
        }
        if let playing = page.playing, playing == 0 || playing > page.total {
            throw ContractError.invalid("The current queue position is inconsistent. Refresh the queue.")
        }
        let before: UInt64
        switch destination {
        case .earlier: before = position - 1
        // Skip the whole original block, then one following occurrence.
        case .later: before = position + count + 1
        case .start: before = 1
        case .end: before = total + 1
        case .afterCurrent:
            guard let playing = page.playing, !(position..<(position + count)).contains(UInt64(playing)) else {
                throw ContractError.invalid("Choose a block outside the current queue entry after its position is known.")
            }
            before = UInt64(playing) + 1
        }
        guard before > 0, before <= total + 1, let insertBefore = UInt32(exactly: before),
              before < position || before > position + count else {
            throw ContractError.invalid("This move would not change the queue order, or its destination is unavailable.")
        }
        self.items = items; sourceURIs = items.map { $0.uri! }; self.destination = destination
        self.revision = revision; self.total = page.total; self.playing = page.playing
        self.insertBefore = insertBefore
        resultingPosition = before < position ? insertBefore : insertBefore - UInt32(count)
    }

    static func validRevision(_ value: String) -> Bool {
        !value.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty && value.utf8.count <= 1_024 &&
        !value.unicodeScalars.contains(where: CharacterSet.controlCharacters.contains)
    }
    static func hasIdentity(_ item: QueueItem) -> Bool {
        guard let uri = item.uri else { return false }
        return item.position > 0 && !uri.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty &&
            uri != "NOT_IMPLEMENTED" && uri.utf8.count <= 16_384
    }
    static func sameOccurrence(_ left: QueueItem, _ right: QueueItem) -> Bool {
        guard left.position == right.position, let a = left.uri, let b = right.uri else { return false }
        return a.utf8.elementsEqual(b.utf8)
    }

    public var explanation: String {
        let placement: String
        switch destination {
        case .earlier: placement = "one place earlier"
        case .later: placement = "one place later"
        case .afterCurrent: placement = "after the current entry at position \(playing!)"
        case .start: placement = "to the start of the queue"
        case .end: placement = "to the end of the queue"
        }
        if count == 1 {
            return "Move “\(item.title)” from position \(item.position) \(placement). This exact queued copy will be at position \(resultingPosition) of \(total).\n\nOnly queue order changes; playback is not started or restarted. Undo requires a complete backup saved by the daemon; check the result."
        }
        let entries = items.map { "\($0.position). \($0.title)" }.joined(separator: "\n")
        return "Move \(count) consecutive entries from positions \(item.position)–\(items.last!.position) \(placement), starting at position \(resultingPosition) of \(total). Their internal order stays unchanged.\n\n\(entries)\n\nOnly queue order changes; playback is not started or restarted. Undo requires a complete backup saved by the daemon; check the result."
    }

    func fields(in zone: Zone) -> [String: JSONValue] {
        ["zone": .string(zone.id.address), "position": .integer(Int64(item.position)),
         "insert_before": .integer(Int64(insertBefore)), "expected_revision": .string(revision),
         "expected_uris": .array(sourceURIs.map(JSONValue.string))]
    }

    private func validateHeader(_ page: QueuePage) throws {
        guard page.revision.map({ $0.utf8.elementsEqual(revision.utf8) }) == true, page.total == total else {
            throw ContractError.invalid("The reviewed queue entry or destination changed. Refresh the queue and choose the move again.")
        }
        if destination == .afterCurrent, page.playing != playing {
            throw ContractError.invalid("The current queue entry changed. Review where to place this block again.")
        }
        if let playing = page.playing, playing == 0 || playing > page.total {
            throw ContractError.invalid("The current queue position is inconsistent. Refresh the queue.")
        }
    }

    /// A fresh observation starts at the selected block, not a containing page.
    func validate(_ page: QueuePage) throws {
        try validateHeader(page)
        _ = try page.validated(offset: item.position - 1, limit: UInt32(Self.maximumCount))
        guard page.items.count >= items.count,
              zip(items, page.items).allSatisfy(Self.sameOccurrence) else {
            throw ContractError.invalid("A reviewed queue occurrence changed. Refresh the queue and select the block again.")
        }
    }

    /// Preparation and confirmation both inspect every selected occurrence.
    /// Sequential 25-entry reads share a revision; at most four reads are made.
    /// The daemon still checks the revision atomically at the single move write.
    func validate(read: @Sendable (UInt32) async throws -> QueuePage) async throws {
        var found: [QueueItem] = []
        for _ in 0..<4 {
            try Task.checkCancellation()
            let offset = item.position - 1 + UInt32(found.count)
            let page = try await read(offset)
            try Task.checkCancellation()
            try validateHeader(page)
            _ = try page.validated(offset: offset)
            let needed = items.count - found.count
            let portion = Array(page.items.prefix(needed))
            guard !portion.isEmpty,
                  zip(items.dropFirst(found.count), portion).allSatisfy(Self.sameOccurrence) else {
                throw ContractError.invalid("A reviewed queue occurrence changed. Refresh the queue and select the block again.")
            }
            found.append(contentsOf: portion)
            if found.count == items.count { return }
        }
        throw ContractError.invalid("The speaker did not return the complete selected block within four page reads. Nothing was sent.")
    }

    func receipt(_ data: Data, in zone: Zone?) throws -> CommandReceipt {
        struct Moved: Decodable {
            let done: String
            let zone: String
            let position: UInt32
            let count: UInt32
            let total: UInt32
            let revision: String
            let changed: Bool
            let recovered: Bool
            let undoable: Bool
        }
        let result = try DaemonClient.decode(Moved.self, from: data)
        guard let zone, result.zone.utf8.elementsEqual(zone.coordinatorRoom.utf8),
              !result.done.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              result.position == resultingPosition, result.count == count, result.total == total,
              Self.validRevision(result.revision), !result.revision.utf8.elementsEqual(revision.utf8),
              result.changed else { throw ClientError.unreadable }
        let recovery = result.recovered ? " The speaker’s result was confirmed by reading the queue again." : ""
        let moved = count == 1 ? "“\(item.title)” moved from position \(item.position) to \(result.position) of \(result.total)."
            : "\(count) entries moved together from positions \(item.position)–\(items.last!.position) to positions \(result.position)–\(UInt64(result.position) + UInt64(count) - 1) of \(result.total), preserving their order."
        let undo = result.undoable
            ? " Undo is available while the queue and playback still match this edit."
            : " This move cannot be undone."
        return CommandReceipt(title: result.done, detail: moved + recovery + undo)
    }
}
