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

/// One exact occurrence in an observed queue. Both wire positions refer to
/// the original queue; no move is ever recalculated against a newer listing.
public struct QueueMove: Equatable, Sendable {
    public let item: QueueItem
    public let destination: QueueMoveDestination
    public let revision: String
    public let total: UInt32
    public let playing: UInt32?
    public let insertBefore: UInt32
    public let resultingPosition: UInt32
    let sourceURI: String

    public init(_ item: QueueItem, in page: QueuePage, to destination: QueueMoveDestination) throws {
        guard item.position > 0, item.position <= page.total, page.items.contains(item),
              let uri = item.uri, !uri.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              uri != "NOT_IMPLEMENTED", let revision = page.revision,
              !revision.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              let first = page.items.first, first.position > 0 else {
            throw ContractError.invalid("Refresh this queue before arranging its entries.")
        }
        _ = try page.validated(offset: first.position - 1)
        if let playing = page.playing, playing == 0 || playing > page.total {
            throw ContractError.invalid("The current queue position is inconsistent. Refresh the queue.")
        }
        let position = UInt64(item.position), total = UInt64(page.total)
        let before: UInt64
        switch destination {
        case .earlier: before = position - 1
        // Inserting immediately after the selected slot is a no-op. Skip
        // that original slot to actually move one place later.
        case .later: before = position + 2
        case .start: before = 1
        case .end: before = total + 1
        case .afterCurrent:
            guard let playing = page.playing, playing != item.position else {
                throw ContractError.invalid("Choose a different entry after a current queue position is known.")
            }
            before = UInt64(playing) + 1
        }
        guard before > 0, before <= total + 1, let insertBefore = UInt32(exactly: before),
              before < position || before > position + 1 else {
            throw ContractError.invalid("This move would not change the queue order, or its destination is unavailable.")
        }
        self.item = item; self.sourceURI = uri; self.destination = destination
        self.revision = revision; self.total = page.total; self.playing = page.playing
        self.insertBefore = insertBefore
        resultingPosition = before < position ? insertBefore : insertBefore - 1
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
        return "Move “\(item.title)” from position \(item.position) \(placement). This exact queued copy will be at position \(resultingPosition) of \(total).\n\nOnly queue order changes; playback is not started or restarted. This move cannot be undone."
    }

    /// Re-run on preparation and confirmation. A queue revision does not bind
    /// the currently playing slot, so after-current also checks that separately.
    func validate(_ page: QueuePage) throws {
        guard page.revision == revision, page.total == total,
              let found = page.items.first, found.position == item.position,
              found.uri == sourceURI, found.playable else {
            throw ContractError.invalid("The reviewed queue entry or destination changed. Refresh the queue and choose the move again.")
        }
        if destination == .afterCurrent, page.playing != playing {
            throw ContractError.invalid("The current queue entry changed. Review where to place this entry again.")
        }
        if let playing = page.playing, playing == 0 || playing > page.total {
            throw ContractError.invalid("The current queue position is inconsistent. Refresh the queue.")
        }
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
        guard let zone, result.zone == zone.coordinatorRoom,
              !result.done.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              result.position == resultingPosition, result.count == 1, result.total == total,
              !result.revision.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              result.revision != revision, result.changed, !result.undoable else { throw ClientError.unreadable }
        let recovery = result.recovered ? " The speaker’s result was confirmed by reading the queue again." : ""
        return CommandReceipt(title: result.done,
                              detail: "“\(item.title)” moved from position \(item.position) to \(result.position) of \(result.total).\(recovery) This move cannot be undone.")
    }
}
