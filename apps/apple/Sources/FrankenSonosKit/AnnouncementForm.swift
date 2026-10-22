import Foundation

/// Local composer state only. Choosing a source, selecting rooms or editing
/// speech never performs I/O; a completed value enters the shared command review.
public struct AnnouncementForm: Sendable {
    public enum Kind: String, CaseIterable, Sendable { case speech, chime, wav }
    public var kind: Kind = .speech {
        didSet { if kind != oldValue { cancelFileImport() } }
    }
    public var speech = ""
    public var chime: Announcement.Chime = .bell
    public var volume = 35
    public private(set) var wav: AnnouncementWav?
    public private(set) var fileLoading = false
    private var fileImport: UUID?
    public private(set) var group: Zone
    public private(set) var availableGroups: [Zone] = []
    public private(set) var selectedRooms: Set<String> = []
    public private(set) var notice: String?

    public init(group: Zone) {
        self.init(group: group, among: [group])
    }

    public init(group: Zone, among topology: [Zone]) {
        self.group = group
        availableGroups = (try? Announcement.householdGroups(in: group, among: topology)) ?? []
    }

    /// Existing single-group callers retain their original recipient scope.
    public mutating func observe(_ current: Zone) { observe(current, among: [current]) }

    /// Playback/member ordering is harmless. A changed household layout retires
    /// recipients and pending file reads, never the typed message or loaded clip.
    /// Returns whether the UI must also cancel its picker/file task.
    @discardableResult public mutating func observe(_ current: Zone, among topology: [Zone]) -> Bool {
        let available = (try? Announcement.householdGroups(in: current, among: topology)) ?? []
        let changed = !Announcement.sameGroup(group, current) ||
            !Announcement.sameLayout(availableGroups, available)
        if changed || available.isEmpty {
            selectedRooms = []
            cancelFileImport()
            notice = available.isEmpty
                ? "The household grouping could not be read. Refresh the house before choosing recipients. Your message was kept."
                : "The household grouping changed. Choose the announcement rooms again. Your message was kept."
        }
        group = current; availableGroups = available
        return changed || available.isEmpty
    }

    public var availableRooms: [String] { availableGroups.flatMap(\.members) }
    public var canSelectHousehold: Bool { !availableRooms.isEmpty && availableRooms.count <= Announcement.maximumRooms }

    @discardableResult public mutating func select(_ room: String, selected: Bool) -> Bool {
        guard availableRooms.contains(where: { $0.utf8.elementsEqual(room.utf8) }),
              !selected || selectedRooms.contains(room) || selectedRooms.count < Announcement.maximumRooms else { return false }
        if selected { selectedRooms.insert(room) } else { selectedRooms.remove(room) }
        notice = nil
        return true
    }
    /// Deliberately does not expand the meaning of the existing group shortcut.
    public mutating func selectAll() {
        guard availableGroups.contains(where: { Announcement.sameGroup($0, group) }) else { return }
        selectedRooms = Set(group.members); notice = nil
    }
    @discardableResult public mutating func selectHousehold() -> Bool {
        guard canSelectHousehold else { return false }
        selectedRooms = Set(availableRooms); notice = nil
        return true
    }
    public mutating func clearRooms() { selectedRooms = []; notice = nil }

    /// Each picker result owns only its own file read. Replacing/removing the
    /// clip, changing source/group, or hiding the panel invalidates old reads.
    public mutating func beginFileImport() -> UUID {
        kind = .wav
        let id = UUID()
        fileImport = id; wav = nil; fileLoading = true
        return id
    }
    @discardableResult public mutating func finishFileImport(_ clip: AnnouncementWav, id: UUID) -> Bool {
        guard fileImport == id, kind == .wav else { return false }
        fileImport = nil; fileLoading = false; wav = clip
        return true
    }
    @discardableResult public mutating func failFileImport(id: UUID) -> Bool {
        guard fileImport == id else { return false }
        cancelFileImport()
        return true
    }
    public mutating func cancelFileImport() { fileImport = nil; fileLoading = false }
    public mutating func removeFile() { cancelFileImport(); wav = nil }

    public func announcement(in current: Zone) throws -> Announcement {
        guard Announcement.sameGroup(group, current) else {
            throw ContractError.invalid("The announcement destination changed. Select its rooms again.")
        }
        let source: Announcement.Source
        switch kind {
        case .speech: source = .speech(speech)
        case .chime: source = .chime(chime)
        case .wav:
            guard !fileLoading, let wav else {
                throw ContractError.invalid("Choose a valid WAV and wait for its local read before reviewing.")
            }
            source = .wav(wav)
        }
        return try Announcement(source: source, rooms: availableRooms.filter { selectedRooms.contains($0) },
                                volume: volume, in: group, among: availableGroups)
    }
}
