import Foundation

/// Local composer state only. Choosing a source, selecting rooms or editing
/// speech never performs I/O; a completed value enters the shared command review.
public struct AnnouncementForm: Sendable {
    public enum Kind: String, CaseIterable, Sendable { case speech, chime }
    public var kind: Kind = .speech
    public var speech = ""
    public var chime: Announcement.Chime = .bell
    public var volume = 35
    public private(set) var group: Zone
    public private(set) var selectedRooms: Set<String> = []
    public private(set) var notice: String?

    public init(group: Zone) { self.group = group }

    /// Playback progress is harmless; a different household, coordinator or
    /// membership requires an explicit new recipient selection.
    public mutating func observe(_ current: Zone) {
        if !group.sameGroup(as: current) {
            selectedRooms = []
            notice = "The selected group changed. Choose the announcement rooms again. Your message was kept."
        }
        group = current
    }
    @discardableResult public mutating func select(_ room: String, selected: Bool) -> Bool {
        guard group.members.contains(where: { $0.utf8.elementsEqual(room.utf8) }) else { return false }
        if selected { selectedRooms.insert(room) } else { selectedRooms.remove(room) }
        notice = nil
        return true
    }
    public mutating func selectAll() { selectedRooms = Set(group.members); notice = nil }
    public mutating func clearRooms() { selectedRooms = []; notice = nil }

    public func announcement(in current: Zone) throws -> Announcement {
        guard group.sameGroup(as: current) else {
            throw ContractError.invalid("The announcement destination changed. Select its rooms again.")
        }
        let source: Announcement.Source = kind == .speech ? .speech(speech) : .chime(chime)
        return try Announcement(source: source, rooms: group.members.filter { selectedRooms.contains($0) },
                                volume: volume, in: group)
    }
}
