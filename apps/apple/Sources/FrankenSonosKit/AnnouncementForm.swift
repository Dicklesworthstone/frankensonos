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
    public private(set) var selectedRooms: Set<String> = []
    public private(set) var notice: String?

    public init(group: Zone) { self.group = group }

    /// Playback progress is harmless; a different household, coordinator or
    /// membership requires an explicit new recipient selection.
    public mutating func observe(_ current: Zone) {
        if !group.sameGroup(as: current) {
            selectedRooms = []
            cancelFileImport()
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
        guard group.sameGroup(as: current) else {
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
        return try Announcement(source: source, rooms: group.members.filter { selectedRooms.contains($0) },
                                volume: volume, in: group)
    }
}
