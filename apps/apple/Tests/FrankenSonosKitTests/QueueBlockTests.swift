import Foundation
import Testing
@testable import FrankenSonosKit

private func blockZone(_ house: String = "S2", members: [String] = ["Studio", "Hall"]) -> Zone {
    Zone(household: house, coordinatorRoom: "Studio", members: members, transportState: "playing")
}
private func blockPage(_ offset: UInt32 = 0, total: UInt32 = 120, size: UInt32 = 25,
                       revision: String = "block:r1", playing: UInt32? = 2) -> QueuePage {
    let count = min(size, total > offset ? total - offset : 0)
    let entries = (0..<count).map { i in
        let position = offset + i + 1
        return QueueItem(position: position, title: "Entry \(position)",
                         uri: position % 2 == 0 ? "renderer:duplicate" : "renderer:\(position)", creator: nil, album: nil)
    }
    return QueuePage(total: total, items: entries, playing: playing, revision: revision)
}
private func blockMove(_ position: UInt32 = 26, count: UInt32 = 60,
                       destination: QueueMoveDestination = .start, playing: UInt32? = 2) throws -> QueueMove {
    let page = blockPage(position - 1, size: count, playing: playing)
    return try QueueMove(page.items, in: page, to: destination)
}

@Suite("Native consecutive queue blocks") struct QueueBlockTests {
    @Test func allDestinationsKeepOriginalCoordinatesAndWholeBlockOrder() throws {
        let page = blockPage(0, total: 10, size: 10, playing: 8)
        let items = Array(page.items[2...4])
        for (destination, before, result) in [
            (QueueMoveDestination.earlier, UInt32(2), UInt32(2)), (.later, 7, 4),
            (.start, 1, 1), (.end, 11, 8), (.afterCurrent, 9, 6)
        ] {
            let move = try QueueMove(items, in: page, to: destination)
            #expect(move.count == 3 && move.insertBefore == before && move.resultingPosition == result)
            #expect(move.sourceURIs == items.map { $0.uri! })
            var reference = Array(1...10)
            let removed = Array(reference[2...4]); reference.removeSubrange(2...4)
            reference.insert(contentsOf: removed, at: Int(result) - 1)
            #expect(Array(reference[(Int(result) - 1)..<(Int(result) + 2)]) == [3, 4, 5])
        }
    }

    @Test func everySmallBlockMatchesRemoveThenInsertReference() throws {
        for total in UInt32(2)...12 {
            let page = blockPage(0, total: total, size: total, playing: total)
            for start in 1...total {
                for count in 1...(total - start + 1) {
                    let selected = Array(page.items[Int(start - 1)..<Int(start - 1 + count)])
                    for destination in QueueMoveDestination.allCases {
                        guard let move = try? QueueMove(selected, in: page, to: destination) else { continue }
                        let original = Array(UInt32(1)...total)
                        let selectedPositions = Array(start..<(start + count))
                        var remaining = original.filter { !selectedPositions.contains($0) }
                        let index = original.prefix(Int(move.insertBefore - 1)).filter { !selectedPositions.contains($0) }.count
                        remaining.insert(contentsOf: selectedPositions, at: index)
                        #expect(move.resultingPosition == UInt32(index + 1))
                        #expect(remaining != original && Set(remaining) == Set(original))
                    }
                }
            }
        }
    }

    @Test func sparseDuplicateReversedUnknownAndOversizedBlocksAreRejected() throws {
        let page = blockPage(0, size: 100)
        for items in [[], [page.items[1], page.items[3]], [page.items[2], page.items[1]], [page.items[1], page.items[1]]] as [[QueueItem]] {
            #expect(throws: ContractError.self) { try QueueMove(items, in: page, to: .end) }
        }
        let large = blockPage(0, size: 101)
        #expect(throws: ContractError.self) { try QueueMove(large.items, in: large, to: .end) }
        for uri in [nil, "", " \n", "NOT_IMPLEMENTED", String(repeating: "x", count: 16_385)] as [String?] {
            let item = QueueItem(position: 1, title: "Unknown", uri: uri, creator: nil, album: nil)
            let unknown = QueuePage(total: 5, items: [item], playing: nil, revision: "r")
            #expect(throws: ContractError.self) { try QueueMove([item], in: unknown, to: .end) }
        }
    }

    @Test func adjacentDestinationsCurrentWithinBlockAndOverflowNeverBecomeMoves() throws {
        let page = blockPage(0, total: 10, size: 10)
        #expect(throws: ContractError.self) { try QueueMove(Array(page.items[0...2]), in: page, to: .earlier) }
        #expect(throws: ContractError.self) { try QueueMove(Array(page.items[7...9]), in: page, to: .later) }
        #expect(throws: ContractError.self) { try QueueMove(Array(page.items[0...9]), in: page, to: .end) }
        #expect(throws: ContractError.self) { try QueueMove(Array(page.items[0...2]), in: page, to: .afterCurrent) }
        #expect(throws: ContractError.self) { try QueueMove(Array(page.items[2...4]), in: page, to: .afterCurrent) }
        let huge = blockPage(.max - 3, total: .max, size: 2, playing: .max)
        for destination in [QueueMoveDestination.end, .later, .afterCurrent] {
            #expect(throws: ContractError.self) { try QueueMove(huge.items, in: huge, to: destination) }
        }
        #expect(try QueueMove(huge.items, in: huge, to: .start).count == 2)
    }

    @Test(arguments: ["S1", "S2"])
    func fullBlockPayloadHasOneOrderedURIRangeAndNoPlaybackFields(house: String) throws {
        let move = try blockMove()
        let fields = move.fields(in: blockZone(house))
        #expect(fields == ["zone": .string("Studio@\(house)"), "position": .integer(26),
                           "insert_before": .integer(1), "expected_revision": .string("block:r1"),
                           "expected_uris": .array(move.items.map { .string($0.uri!) })])
        #expect(move.explanation.contains("60 consecutive entries") && move.explanation.contains("Entry 85"))
        #expect(move.explanation.contains("internal order stays unchanged"))
    }

    @Test func singleEntryCompatibilityRemainsIntact() throws {
        let page = blockPage(0, total: 6)
        let move = try QueueMove(page.items[4], in: page, to: .end)
        #expect(move.item == page.items[4] && move.count == 1 && move.insertBefore == 7 && move.resultingPosition == 6)
        #expect(move.sourceURI == move.sourceURIs[0])
        #expect(move.explanation.contains("position 6 of 6"))
        try move.validate(blockPage(4, total: 6))
    }

    @Test func freshValidationChecksEveryOccurrenceAndExactBytes() throws {
        let move = try blockMove(1, count: 50, destination: .end)
        let fresh = blockPage(0, size: 50)
        try move.validate(fresh)
        var changed = fresh.items
        changed[49] = QueueItem(position: 50, title: "changed", uri: "renderer:other", creator: nil, album: nil)
        #expect(throws: ContractError.self) { try move.validate(QueuePage(total: 120, items: changed, playing: 2, revision: "block:r1")) }
        #expect(throws: ContractError.self) { try move.validate(blockPage(0)) }
        let item = QueueItem(position: 1, title: "Canonical", uri: "renderer:é", creator: nil, album: nil)
        let page = QueuePage(total: 2, items: [item], playing: nil, revision: "révision")
        let single = try QueueMove(item, in: page, to: .end)
        let other = QueueItem(position: 1, title: "Canonical", uri: "renderer:e\u{301}", creator: nil, album: nil)
        #expect(throws: ContractError.self) { try single.validate(QueuePage(total: 2, items: [other], playing: nil, revision: "révision")) }
        #expect(throws: ContractError.self) { try single.validate(QueuePage(total: 2, items: [item], playing: nil, revision: "re\u{301}vision")) }
    }

    @Test func preflightReadsEntireHundredEntryBlockInFourSequentialPages() async throws {
        let move = try blockMove(7, count: 100, destination: .start)
        let reader = BlockReader()
        try await move.validate { try await reader.page($0) }
        #expect(await reader.offsets == [6, 31, 56, 81])
        #expect(await reader.maxActive == 1)
    }

    @Test func changedLaterPageAndAnchorAbortTheRemainingReads() async throws {
        for fault in [BlockReader.Fault.revision, .identity, .total, .anchor] {
            let reader = BlockReader(fault: fault)
            let move = try blockMove(destination: .afterCurrent)
            await #expect(throws: ContractError.self) { try await move.validate { try await reader.page($0) } }
            #expect(await reader.offsets == [25, 50])
        }
    }

    @Test func cancellationStopsUnsentReadsAndDiscardsLatePage() async throws {
        let reader = BlockReader(held: true), move = try blockMove()
        let task = Task { try await move.validate { try await reader.page($0) } }
        let deadline = ContinuousClock.now.advanced(by: .seconds(3))
        while !(await reader.waiting) {
            guard ContinuousClock.now < deadline else { task.cancel(); throw ContractError.invalid("Block reader did not start") }
            await Task.yield()
        }
        task.cancel(); await reader.release()
        await #expect(throws: CancellationError.self) { try await task.value }
        #expect(await reader.offsets == [25])
    }

    @Test func shortPagesCannotCauseAnUnboundedReadLoop() async throws {
        let move = try blockMove(), reader = BlockReader(fault: .short)
        await #expect(throws: ContractError.self) { try await move.validate { try await reader.page($0) } }
        #expect(await reader.offsets == [25, 26, 27, 28])
    }

    @Test func blockReceiptRequiresTheEntireMovedRange() throws {
        let move = try blockMove()
        let base: [String: JSONValue] = ["done": .string("Moved"), "zone": .string("Studio"), "position": .integer(1),
            "count": .integer(60), "total": .integer(120), "revision": .string("block:r2"), "changed": .bool(true),
            "recovered": .bool(true), "undoable": .bool(false)]
        let receipt = try move.receipt(JSONEncoder().encode(base), in: blockZone())
        #expect(receipt.detail.contains("positions 1–60") && receipt.detail.contains("confirmed by reading"))
        #expect(!receipt.partial)
        for (key, value) in [("count", JSONValue.integer(1)), ("position", .integer(2)), ("total", .integer(121)),
                             ("revision", .string("block:r1")), ("zone", .string("Other")), ("undoable", .bool(true)), ("changed", .bool(false))] {
            var wrong = base; wrong[key] = value
            #expect(throws: ClientError.self) { try move.receipt(JSONEncoder().encode(wrong), in: blockZone()) }
        }
    }
}

private actor BlockReader {
    enum Fault: Sendable { case none, revision, identity, total, anchor, short }
    var offsets: [UInt32] = []
    var maxActive = 0, active = 0
    var waiting = false
    let fault: Fault
    let held: Bool
    var gate: CheckedContinuation<Void, Never>?
    init(fault: Fault = .none, held: Bool = false) { self.fault = fault; self.held = held }
    func release() { gate?.resume(); gate = nil }
    func page(_ offset: UInt32) async throws -> QueuePage {
        offsets.append(offset); active += 1; maxActive = max(maxActive, active)
        defer { active -= 1 }
        if held && offsets.count == 1 { waiting = true; await withCheckedContinuation { gate = $0 } }
        let bad = offsets.count == 2
        let page = blockPage(offset, total: bad && fault == .total ? 121 : 120,
                             size: fault == .short ? 1 : 25, revision: bad && fault == .revision ? "new" : "block:r1",
                             playing: bad && fault == .anchor ? 3 : 2)
        if bad && fault == .identity {
            var items = page.items
            items[items.count - 1] = QueueItem(position: items.last!.position, title: "Replaced", uri: "renderer:other", creator: nil, album: nil)
            return QueuePage(total: page.total, items: items, playing: page.playing, revision: page.revision)
        }
        return page
    }
}

@Suite("Native cross-page queue selection") struct QueueSelectionTests {
    @Test func selectionAcrossPagesKeepsOnlyChosenEntriesAndTheirOriginalOrder() throws {
        var selected = QueueSelection()
        let a = blockPage(), b = blockPage(25)
        try selected.selectPage(a, in: blockZone(), offset: 0)
        try selected.observe(b, in: blockZone(), offset: 25)
        #expect(selected.items.count == 25)
        try selected.selectPage(b, in: blockZone(), offset: 25)
        #expect(selected.items.count == 50 && selected.isContiguous)
        let move = try selected.move(to: .end)
        #expect(move.count == 50 && move.resultingPosition == 71)
        #expect(move.items[1].uri == move.items[3].uri && move.items[1].position != move.items[3].position)
    }
    @Test func gapsNeverIncludeUnselectedEntriesAndCanBeFilledExplicitly() throws {
        var selected = QueueSelection(); let page = blockPage()
        try selected.toggle(page.items[0], in: page, zone: blockZone(), offset: 0)
        try selected.toggle(page.items[2], in: page, zone: blockZone(), offset: 0)
        #expect(selected.items.count == 2 && !selected.isContiguous)
        #expect(throws: ContractError.self) { try selected.move(to: .end) }
        try selected.toggle(page.items[1], in: page, zone: blockZone(), offset: 0)
        #expect(try selected.move(to: .end).items.map(\.position) == [1, 2, 3])
    }
    @Test func revisedResizedOrRegroupedQueuesRetireSelection() throws {
        for change in 0..<4 {
            var selected = QueueSelection(); let page = blockPage()
            try selected.selectPage(page, in: blockZone(), offset: 0)
            let token = selected.id
            let newPage = blockPage(25, total: change == 1 ? 121 : 120, revision: change == 0 ? "new" : "block:r1")
            let zone = blockZone(change == 2 ? "S1" : "S2", members: change == 3 ? ["Studio"] : ["Studio", "Hall"])
            try selected.observe(newPage, in: zone, offset: 25)
            #expect(selected.isEmpty && selected.id != token)
            #expect(!selected.isCurrent(token, page: page, zone: blockZone()))
        }
    }
    @Test func invalidObservationsClearInsteadOfRetainingActionableRows() throws {
        var selected = QueueSelection(); let page = blockPage()
        try selected.selectPage(page, in: blockZone(), offset: 0)
        let invalid = QueuePage(total: page.total, items: [page.items[0], page.items[2]], playing: 2, revision: "block:r1")
        #expect(throws: ContractError.self) { try selected.observe(invalid, in: blockZone(), offset: 0) }
        #expect(selected.isEmpty)
    }
    @Test func selectionIsBoundedAndPageAdditionIsAllOrNothing() throws {
        var selected = QueueSelection()
        for offset in [UInt32(0), 25, 50, 75] { try selected.selectPage(blockPage(offset), in: blockZone(), offset: offset) }
        let token = selected.id
        #expect(throws: ContractError.self) { try selected.selectPage(blockPage(100), in: blockZone(), offset: 100) }
        #expect(selected.items.count == 100 && selected.id == token)
        let page = blockPage()
        try selected.toggle(page.items[0], in: page, zone: blockZone(), offset: 0)
        #expect(selected.items.count == 99 && !selected.isCurrent(token, page: page, zone: blockZone()))
    }
    @Test func missingIdentityIsNeverSilentlySkippedBySelectPage() throws {
        var selected = QueueSelection(); let page = blockPage()
        try selected.toggle(page.items[0], in: page, zone: blockZone(), offset: 0)
        let unknown = QueueItem(position: 26, title: "Unknown", uri: nil, creator: nil, album: nil)
        let next = QueuePage(total: 120, items: [unknown], playing: 2, revision: "block:r1")
        #expect(throws: ContractError.self) { try selected.selectPage(next, in: blockZone(), offset: 25) }
        #expect(selected.items.map(\.position) == [1])
    }
    @Test func sameRevisionIdentityReplacementRetiresRatherThanRetargetsSelection() throws {
        var selected = QueueSelection(); let page = blockPage()
        try selected.toggle(page.items[0], in: page, zone: blockZone(), offset: 0)
        var items = page.items
        items[0] = QueueItem(position: 1, title: "Replaced", uri: "other", creator: nil, album: nil)
        #expect(throws: ContractError.self) { try selected.observe(QueuePage(total: 120, items: items, playing: 2, revision: "block:r1"), in: blockZone(), offset: 0) }
        #expect(selected.isEmpty)
    }
    @Test func clockProgressKeepsSelectionButNewCurrentSlotRetiresMenus() throws {
        var selected = QueueSelection(); let page = blockPage()
        try selected.toggle(page.items[5], in: page, zone: blockZone(), offset: 0)
        let token = selected.id
        try selected.observe(page, in: blockZone(members: ["Hall", "Studio"]), offset: 0)
        #expect(selected.isCurrent(token, page: page, zone: blockZone()))
        try selected.observe(blockPage(playing: 3), in: blockZone(), offset: 0)
        #expect(!selected.isCurrent(token, page: page, zone: blockZone()))
        #expect(selected.items.count == 1 && selected.playing == 3)
        selected.clear(); #expect(selected.isEmpty && selected.zone == nil)
    }
}
