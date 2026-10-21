import Foundation

/// Editing is entirely local. A draft imports only a session's own additions,
/// never its mood's inherited filters. Saving explicitly replaces the session.
public struct DJSteeringDraft: Sendable {
    public let group: Zone
    public let before: DJStatus.Steering
    public var mood = ""
    public var entries: [DJSteeringField: String] = [:]
    public var minimumMinutes = "", maximumMinutes = ""
    public var energyBias = 0
    public var allowLongWorks = false
    public var untilCleared = false
    public var minutes = "60"

    public init(in group: Zone, status: DJStatus) throws {
        _ = try group.validated(); _ = try status.validated(for: group)
        let baseline = try DJSteeringBaseline(status.steering)
        self.group = group; before = status.steering
        let fields = baseline.source == "session" ? baseline.added.values : [:]
        mood = baseline.source == "session" ? baseline.mood ?? "" : ""
        loadFilters(fields)
    }

    /// Copy explicit preview settings into a NEW local draft. The current
    /// session is the replacement baseline, never the source of extra filters.
    /// A seed, planned start, set length and predicted queue are not steering.
    public init(rehearsal: DJRehearsalRequest, in group: Zone, status: DJStatus,
                moods: [MoodCatalog.Mood]) throws {
        guard group.sameGroup(as: rehearsal.zone) else {
            throw ContractError.invalid("The preview belongs to a different group. Generate again for the selected rooms.")
        }
        let filters = try DJSteeringFilters(rehearsal.constraints)
        guard rehearsal.mood != nil || !filters.values.isEmpty else {
            throw ContractError.invalid("This preview inherited live steering. It has no explicit mood or filters to copy.")
        }
        let selected: String
        if let raw = rehearsal.mood {
            // The rehearsal engine lowercases a supplied mood before resolving
            // it. Use that exact canonical name, not fuzzy or Unicode matching.
            let name = try DJSteeringFilters.word(raw).lowercased()
            let matches = moods.filter { $0.name.utf8.elementsEqual(name.utf8) }
            guard matches.count == 1 else {
                throw ContractError.invalid("The preview mood is missing or ambiguous. Refresh the DJ moods before review.")
            }
            _ = try DJSteeringFilters(matches[0].constraints)
            selected = matches[0].name
        } else { selected = "" }
        try self.init(in: group, status: status)
        loadFilters(filters.values)
        mood = selected
        // Preview hours are NOT a session lifetime. Require an explicit new
        // duration, or the separate until-cleared choice, before review.
        minutes = ""; untilCleared = false
    }

    private mutating func loadFilters(_ fields: [String: JSONValue]) {
        entries = [:]; minimumMinutes = ""; maximumMinutes = ""
        energyBias = 0; allowLongWorks = false
        for field in DJSteeringField.allCases {
            guard case .array(let values) = fields[field.rawValue] else { continue }
            entries[field] = values.map { value -> String in
                switch value { case .string(let text): text; case .integer(let number): String(number); default: "" }
            }.joined(separator: "\n")
        }
        if case .integer(let value) = fields["min_work_minutes"] { minimumMinutes = String(value) }
        if case .integer(let value) = fields["max_work_minutes"] { maximumMinutes = String(value) }
        if case .integer(let value) = fields["energy_bias"] { energyBias = Int(value) }
        allowLongWorks = fields["allow_long_works"] == .bool(true)
    }

    public func change(moods: [MoodCatalog.Mood]) throws -> DJSteeringChange {
        var fields: [String: JSONValue] = [:]
        for field in DJSteeringField.allCases {
            let lines = (entries[field] ?? "").components(separatedBy: .newlines)
                .map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }.filter { !$0.isEmpty }
            if !lines.isEmpty {
                fields[field.rawValue] = .array(try lines.map { line in
                    field.isDecade ? .integer(try Self.number(line, range: 1000...65535, name: field.title)) : .string(line)
                })
            }
        }
        for (text, key) in [(minimumMinutes, "min_work_minutes"), (maximumMinutes, "max_work_minutes")] {
            if !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                fields[key] = .integer(try Self.number(text, range: 1...600, name: "Work length"))
            }
        }
        fields["energy_bias"] = .integer(Int64(energyBias))
        fields["allow_long_works"] = .bool(allowLongWorks)
        let selected: MoodCatalog.Mood?
        if mood.isEmpty { selected = nil }
        else {
            let matches = moods.filter { $0.name.utf8.elementsEqual(mood.utf8) }
            guard matches.count == 1 else { throw ContractError.invalid("Choose a mood returned by this daemon, or the automatic program.") }
            selected = matches[0]
        }
        let seconds = untilCleared ? nil : UInt32(try Self.number(minutes, range: 1...10080, name: "Duration")) * 60
        return try DJSteeringChange(in: group, before: before, mood: selected,
                                    filters: DJSteeringFilters(fields), seconds: seconds)
    }
    public func clear() throws -> DJSteeringChange { try .clear(in: group, before: before) }

    private static func number(_ raw: String, range: ClosedRange<Int64>, name: String) throws -> Int64 {
        let text = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty, text.utf8.allSatisfy({ (48...57).contains($0) }),
              let value = Int64(text), range.contains(value) else {
            throw ContractError.invalid("\(name) must be a whole number from \(range.lowerBound) to \(range.upperBound).")
        }
        return value
    }
}
