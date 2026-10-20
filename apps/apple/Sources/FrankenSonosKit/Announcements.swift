import Foundation

/// An explicit, reviewed announcement to rooms of one selected group. Never
/// omit recipients: the daemon interprets an empty room list as the whole house.
public struct Announcement: Equatable, Sendable {
    public enum Chime: String, CaseIterable, Sendable { case bell, beep, rise }
    public enum Source: Equatable, Sendable {
        case speech(String)
        case chime(Chime)
        case wav(AnnouncementWav)
    }
    public static let maximumSpeechScalars = 1_000
    public static let timeout: TimeInterval = 1_200
    public let source: Source
    public let rooms: [String]
    public let volume: Int
    public let group: Zone

    public init(source: Source, rooms: [String], volume: Int = 35, in group: Zone) throws {
        _ = try group.validated()
        guard !rooms.isEmpty, rooms.count <= 256, Set(rooms).count == rooms.count,
              rooms.allSatisfy({ room in group.members.contains { $0.utf8.elementsEqual(room.utf8) } }) else {
            throw ContractError.invalid("Select at least one distinct room from this group for the announcement.")
        }
        guard (0...100).contains(volume) else {
            throw ContractError.invalid("Announcement volume must be between 0 and 100.")
        }
        switch source {
        case .speech(let raw):
            let text = raw.trimmingCharacters(in: .whitespacesAndNewlines)
            guard !text.isEmpty, text.unicodeScalars.count <= Self.maximumSpeechScalars,
                  !text.unicodeScalars.contains(where: { $0.value == 0 }) else {
                throw ContractError.invalid("Enter speech of 1 to 1000 Unicode characters, without a null character.")
            }
            self.source = .speech(text)
        case .chime, .wav: self.source = source
        }
        self.rooms = rooms; self.volume = volume; self.group = group
    }

    public var title: String {
        switch source {
        case .speech: "Speak an announcement"
        case .chime(let chime): "Play the \(chime.rawValue) chime"
        case .wav: "Play an audio announcement"
        }
    }
    public var explanation: String {
        let clip: String
        switch source {
        case .speech(let text): clip = "Speak exactly:\n\(text)"
        case .chime(let chime): clip = "Chime: \(chime.rawValue)"
        case .wav(let wav): clip = "Upload exactly this WAV:\n\(wav.summary)\nThe reviewed bytes are retained; the file will not be reopened when you confirm."
        }
        let targets = rooms.map { RoomID(household: group.household, room: $0).address }.joined(separator: ", ")
        return "\(clip)\n\nOnly in: \(targets)\nRequested volume: \(volume) (house caps still apply).\n\nThe daemon temporarily interrupts these rooms and may regroup them, then attempts to restore the original music, groups and levels. Other members of their group may also be affected during restoration. Speech is generated on the daemon with its configured voice. This is not undoable. Closing the remote does not recall a sent announcement."
    }

    /// Reused by NativeCommand's payload validation, including before review.
    func fields(in current: Zone) throws -> [String: JSONValue] {
        _ = try current.validated()
        guard current.sameGroup(as: group) else {
            throw ContractError.invalid("The announcement's group changed. Select its rooms and review again.")
        }
        // Use the retained names and household, not a later UI selection.
        var body: [String: JSONValue] = [
            "rooms": .array(rooms.map { .string(RoomID(household: group.household, room: $0).address) }),
            "volume": .integer(Int64(volume)),
        ]
        switch source {
        case .speech(let text): body["text"] = .string(text)
        case .chime(let chime): body["chime"] = .string(chime.rawValue)
        case .wav(let wav): body["wav_base64"] = .string(wav.base64)
        }
        return body
    }
}

/// The public /announce reply is not an Outcome. It separately reports clip
/// completion, policy-limited levels, and failures to restore the original music.
public struct AnnouncementReport: Decodable, Equatable, Sendable {
    public let done: String
    public let clean: Bool
    public let households: [Household]
    public struct Household: Decodable, Equatable, Sendable {
        public enum Ending: String, Decodable, Sendable { case finished, deadline, neverStarted = "never_started", failed }
        public let lead: String
        public let rooms: [String]
        public let levels: [Level]
        public let outcome: Ending
        public let error: String?
        public let notRestored: [String]?
    }
    public struct Level: Decodable, Equatable, Sendable {
        public let room: String
        public let volume: Int
    }

    public func validated(for announcement: Announcement) throws -> Self {
        func text(_ value: String) -> Bool {
            !value.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        }
        func identities(_ names: [String]) -> Set<Data> { Set(names.map { Data($0.utf8) }) }
        let expected = identities(announcement.rooms)
        // This native command deliberately addresses only one household.
        guard text(done), households.count == 1, let household = households.first,
              text(household.lead), household.rooms.count == announcement.rooms.count,
              identities(household.rooms) == expected,
              household.levels.count <= announcement.rooms.count,
              identities(household.levels.map(\.room)).count == household.levels.count,
              household.levels.allSatisfy({ expected.contains(Data($0.room.utf8)) && (0...100).contains($0.volume) }),
              (household.notRestored ?? []).allSatisfy(text),
              household.error.map(text) ?? true,
              (household.outcome == .failed) == (household.error != nil) else {
            throw ContractError.invalid("The announcement reply did not confirm the reviewed rooms and outcome.")
        }
        if clean {
            guard household.outcome == .finished, (household.notRestored ?? []).isEmpty,
                  household.levels.count == announcement.rooms.count else {
                throw ContractError.invalid("The announcement reply contradicts its completion or restoration status.")
            }
        }
        return self
    }

    public var detail: String {
        var lines = [done]
        for household in households {
            let ending: String
            switch household.outcome {
            case .finished: ending = "Clip finished"
            case .deadline: ending = "Clip stopped at its deadline"
            case .neverStarted: ending = "Clip never started; the speakers could not fetch it"
            case .failed: ending = "Announcement failed"
            }
            lines.append("\(household.rooms.joined(separator: ", ")): \(ending)")
            lines += household.levels.map { "\($0.room): announcement volume \($0.volume) after house policy" }
            if let error = household.error { lines.append("Failed: \(error)") }
            lines += (household.notRestored ?? []).map { "Not restored: \($0)" }
        }
        if !clean { lines.append("Check the affected rooms before another command. The announcement was not repeated.") }
        return lines.joined(separator: "\n")
    }
}
