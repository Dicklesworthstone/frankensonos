import Foundation

/// Bounded, household-scoped selection across ordinary 25-entry queue pages.
/// Only observed occurrences are retained. Gaps are never filled implicitly.
public struct QueueSelection: Equatable, Sendable {
    public private(set) var id = UUID()
    public private(set) var zone: Zone?
    public private(set) var revision: String?
    public private(set) var total: UInt32 = 0
    public private(set) var playing: UInt32?
    public private(set) var items: [QueueItem] = []
    public var isEmpty: Bool { items.isEmpty }
    public var isContiguous: Bool {
        guard let first = items.first else { return false }
        return items.enumerated().allSatisfy { UInt64($0.element.position) == UInt64(first.position) + UInt64($0.offset) }
    }
    public init() {}
    public static func canSelect(_ item: QueueItem) -> Bool { QueueMove.hasIdentity(item) }

    public mutating func clear() { self = Self() }

    /// A refresh can keep a selection only under the same revision and group.
    /// Matching page overlaps are refreshed cosmetically, never reassigned by URI.
    public mutating func observe(_ page: QueuePage, in zone: Zone, offset: UInt32) throws {
        do {
            _ = try zone.validated()
            _ = try page.validated(offset: offset)
            guard let revision = page.revision, QueueMove.validRevision(revision),
                  page.playing.map({ $0 > 0 && $0 <= page.total }) != false else {
                throw ContractError.invalid("Refresh the queue before selecting entries.")
            }
            if self.zone?.sameGroup(as: zone) != true || self.total != page.total ||
                self.revision.map({ $0.utf8.elementsEqual(revision.utf8) }) != true {
                clear()
            }
            var observed = items
            for index in observed.indices {
                guard let found = page.items.first(where: { $0.position == observed[index].position }) else { continue }
                guard QueueMove.sameOccurrence(found, observed[index]) else {
                    throw ContractError.invalid("A selected occurrence changed without a matching revision. Select the block again.")
                }
                observed[index] = found
            }
            if observed != items || playing != page.playing { id = UUID() }
            self.zone = zone; self.revision = revision; total = page.total; playing = page.playing; items = observed
        } catch {
            clear()
            throw error
        }
    }

    public func contains(_ item: QueueItem) -> Bool { items.contains { QueueMove.sameOccurrence($0, item) } }

    public mutating func toggle(_ item: QueueItem, in page: QueuePage, zone: Zone, offset: UInt32) throws {
        try observe(page, in: zone, offset: offset)
        guard QueueMove.hasIdentity(item), page.items.contains(where: { QueueMove.sameOccurrence($0, item) }) else {
            throw ContractError.invalid("This occurrence has no usable queue identity.")
        }
        if let index = items.firstIndex(where: { $0.position == item.position }) { items.remove(at: index) }
        else {
            guard items.count < QueueMove.maximumCount else {
                throw ContractError.invalid("Select at most 100 entries for one move.")
            }
            items.append(item); items.sort { $0.position < $1.position }
        }
        id = UUID()
    }

    /// All-or-nothing page selection; never skip an unidentifiable track silently.
    public mutating func selectPage(_ page: QueuePage, in zone: Zone, offset: UInt32) throws {
        try observe(page, in: zone, offset: offset)
        guard !page.items.isEmpty, page.items.allSatisfy(QueueMove.hasIdentity) else {
            throw ContractError.invalid("This page includes an entry without a usable queue identity. Select known entries individually.")
        }
        var chosen = items
        for item in page.items where !chosen.contains(where: { $0.position == item.position }) { chosen.append(item) }
        guard chosen.count <= QueueMove.maximumCount else {
            throw ContractError.invalid("Select at most 100 entries for one move.")
        }
        items = chosen.sorted { $0.position < $1.position }; id = UUID()
    }

    public func move(to destination: QueueMoveDestination) throws -> QueueMove {
        guard zone != nil, isContiguous else {
            throw ContractError.invalid("Select consecutive entries; unselected tracks are never included in a move.")
        }
        return try QueueMove(items, in: QueuePage(total: total, items: items, playing: playing, revision: revision), to: destination)
    }

    /// Menus retain this token along with their group and complete move value.
    public func isCurrent(_ token: UUID, page: QueuePage, zone: Zone) -> Bool {
        id == token && self.zone?.sameGroup(as: zone) == true && total == page.total &&
            revision.map { expected in page.revision.map { $0.utf8.elementsEqual(expected.utf8) } == true } == true
    }
}
