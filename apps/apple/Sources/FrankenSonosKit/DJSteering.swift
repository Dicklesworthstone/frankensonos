import Foundation

/// The daemon's temporary, group-scoped filters, not standing taste settings.
public enum DJSteeringField: String, CaseIterable, Sendable, Identifiable {
    case includeArtists = "include_artists", excludeArtists = "exclude_artists"
    case includeGenres = "include_genres", excludeGenres = "exclude_genres"
    case decades, excludeDecades = "exclude_decades"
    case includeKeywords = "include_keywords", excludeKeywords = "exclude_keywords"
    case includeComposers = "include_composers", excludeComposers = "exclude_composers"
    case periods, excludePeriods = "exclude_periods"
    public var id: String { rawValue }
    public var title: String {
        switch self {
        case .includeArtists: "Only artists"
        case .excludeArtists: "Exclude artists"
        case .includeGenres: "Only genres"
        case .excludeGenres: "Exclude genres"
        case .decades: "Only release decades"
        case .excludeDecades: "Exclude release decades"
        case .includeKeywords: "Include keywords"
        case .excludeKeywords: "Exclude keywords"
        case .includeComposers: "Only composers"
        case .excludeComposers: "Exclude composers"
        case .periods: "Only classical periods"
        case .excludePeriods: "Exclude classical periods"
        }
    }
    public var isDecade: Bool { self == .decades || self == .excludeDecades }
    public var isPeriod: Bool { self == .periods || self == .excludePeriods }
}

public struct DJSteeringFilters: Equatable, Sendable {
    public static let periods = ["medieval", "renaissance", "baroque", "classical", "romantic",
                                 "late_romantic", "impressionist", "modern", "contemporary"]
    public let values: [String: JSONValue]

    /// Decode sparse serde defaults without dropping unknown or malformed fields.
    public init(_ fields: [String: JSONValue] = [:]) throws {
        let allowed = Set(DJSteeringField.allCases.map(\.rawValue) +
            ["min_work_minutes", "max_work_minutes", "energy_bias", "allow_long_works"])
        guard Set(fields.keys).isSubset(of: allowed) else {
            throw ContractError.invalid("Unknown DJ filter. Use a compatible client before replacing these settings.")
        }
        var result: [String: JSONValue] = [:]
        for field in DJSteeringField.allCases {
            guard let raw = fields[field.rawValue] else { continue }
            guard case .array(let items) = raw, items.count <= 32 else {
                throw ContractError.invalid("\(field.title) accepts at most 32 entries.")
            }
            let cleaned: [JSONValue] = try items.map { item in
                if field.isDecade {
                    guard case .integer(let year) = item, (1000...65535).contains(year), year % 10 == 0 else {
                        throw ContractError.invalid("Release decades use their first year, such as 1960 or 1980.")
                    }
                    return item
                }
                guard case .string(let raw) = item else { throw ContractError.invalid("\(field.title) needs text entries.") }
                let word = try Self.word(raw)
                if field.isPeriod {
                    let period = word.lowercased().replacingOccurrences(of: "-", with: "_").replacingOccurrences(of: " ", with: "_")
                    guard Self.periods.contains(period) else { throw ContractError.invalid("Unknown classical period: \(word).") }
                    return .string(period)
                }
                return .string(word)
            }
            if !cleaned.isEmpty { result[field.rawValue] = .array(cleaned) }
        }
        for key in ["min_work_minutes", "max_work_minutes"] {
            guard let raw = fields[key], raw != .null else { continue }
            guard case .integer(let value) = raw, (1...600).contains(value) else {
                throw ContractError.invalid("Work lengths must be whole minutes from 1 to 600.")
            }
            result[key] = raw
        }
        if case .integer(let minimum) = result["min_work_minutes"],
           case .integer(let maximum) = result["max_work_minutes"], minimum > maximum {
            throw ContractError.invalid("Minimum work length exceeds maximum.")
        }
        if let raw = fields["energy_bias"] {
            guard case .integer(let value) = raw, (-2...2).contains(value) else {
                throw ContractError.invalid("Energy shift must be between -2 and 2.")
            }
            if value != 0 { result["energy_bias"] = raw }
        }
        if let raw = fields["allow_long_works"] {
            guard case .bool(let allowed) = raw else { throw ContractError.invalid("Long-work permission must be a boolean.") }
            if allowed { result["allow_long_works"] = raw }
        }
        values = result
    }

    static func word(_ raw: String) throws -> String {
        let word = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !word.isEmpty, word.unicodeScalars.count <= 128,
              !word.unicodeScalars.contains(where: { $0.value == 0 || $0.value == 10 || $0.value == 13 }) else {
            throw ContractError.invalid("Filter names need 1–128 Unicode characters on one line.")
        }
        return word
    }

    public var summary: String {
        var lines: [String] = []
        for field in DJSteeringField.allCases {
            guard case .array(let items) = values[field.rawValue] else { continue }
            let words = items.map { item -> String in
                switch item { case .string(let v): v; case .integer(let v): String(v); default: "" }
            }
            lines.append("\(field.title): \(words.joined(separator: "; "))")
        }
        for (key, title) in [("min_work_minutes", "Minimum work minutes"), ("max_work_minutes", "Maximum work minutes"), ("energy_bias", "Energy shift")] {
            if case .integer(let value) = values[key] { lines.append("\(title): \(value)") }
        }
        if values["allow_long_works"] == .bool(true) { lines.append("Allow long works") }
        return lines.isEmpty ? "No additional filters" : lines.joined(separator: "\n")
    }
}

/// Compare stored steering, not a decreasing countdown or advancing DJ tracks.
struct DJSteeringBaseline: Equatable, Sendable {
    let source: String, mood: String?
    let expiresAt: Int64?
    let effective: DJSteeringFilters, added: DJSteeringFilters
    init(_ value: DJStatus.Steering) throws {
        guard ["session", "program", "none"].contains(value.source) else {
            throw ContractError.invalid("The current steering source is unknown. Refresh the DJ.")
        }
        source = value.source; mood = try value.mood.map(DJSteeringFilters.word)
        expiresAt = value.expiresAt
        effective = try DJSteeringFilters(value.constraints ?? [:])
        added = try DJSteeringFilters(value.added ?? [:])
    }
}

/// An immutable replacement or explicit clear, bound to the edited session.
public struct DJSteeringChange: Equatable, Sendable {
    public let group: Zone
    public let mood: MoodCatalog.Mood?
    public let filters: DJSteeringFilters
    public let seconds: UInt32?
    public let clearing: Bool
    private let before: DJSteeringBaseline

    public init(in group: Zone, before: DJStatus.Steering, mood: MoodCatalog.Mood?,
                filters: DJSteeringFilters, seconds: UInt32?) throws {
        _ = try group.validated()
        self.before = try DJSteeringBaseline(before)
        if let mood {
            _ = try DJSteeringFilters.word(mood.name)
            _ = try DJSteeringFilters(mood.constraints)
        }
        guard mood != nil || !filters.values.isEmpty else {
            throw ContractError.invalid("Choose a mood or temporary filter, or explicitly clear steering.")
        }
        guard seconds == nil || (60...604_800).contains(seconds!) else {
            throw ContractError.invalid("Steering lasts from one minute to one week, or until cleared.")
        }
        self.group = group; self.mood = mood; self.filters = filters; self.seconds = seconds; clearing = false
    }
    private init(clearing group: Zone, before: DJStatus.Steering) throws {
        _ = try group.validated()
        self.before = try DJSteeringBaseline(before)
        self.group = group; mood = nil; filters = try DJSteeringFilters(); seconds = nil; clearing = true
    }
    public static func clear(in group: Zone, before: DJStatus.Steering) throws -> Self {
        try Self(clearing: group, before: before)
    }
    public var title: String { clearing ? "Clear temporary DJ steering" : "Replace temporary DJ steering" }
    public var explanation: String {
        if clearing { return "Return this group to its automatic program. Standing taste preferences and already queued music stay unchanged." }
        let lifetime = seconds.map { "\($0 / 60) minutes from saving" } ?? "until explicitly cleared"
        return "Group: \(group.id.address)\nMood: \(mood?.name ?? "automatic program")\n\(filters.summary)\nDuration: \(lifetime).\n\nThis replaces the entire temporary session, not just the fields you edited. It shapes future picks without starting the DJ or replacing queued works. Standing taste preferences stay unchanged."
    }
    public func fields(in current: Zone) throws -> [String: JSONValue] {
        _ = try current.validated()
        guard current.sameGroup(as: group) else { throw ContractError.invalid("The DJ group changed. Reopen its settings.") }
        _ = try DJSteeringFilters.word(group.id.address)
        if clearing { return ["zone": .string(group.id.address), "clear": .bool(true)] }
        var body: [String: JSONValue] = ["zone": .string(group.id.address), "constraints": .object(filters.values)]
        if let mood { body["mood"] = .string(mood.name) }
        if let seconds { body["for_secs"] = .integer(Int64(seconds)) }
        return body
    }
    public func validate(_ current: DJStatus) throws {
        _ = try current.validated(for: group)
        guard try DJSteeringBaseline(current.steering) == before else {
            throw ContractError.invalid("Temporary DJ steering changed while you edited it. Reload and review again.")
        }
    }
    public func validateMoods(_ current: [MoodCatalog.Mood]) throws {
        guard let mood else { return }
        let matches = current.filter { $0.name.utf8.elementsEqual(mood.name.utf8) }
        guard matches.count == 1,
              try DJSteeringFilters(matches[0].constraints) == DJSteeringFilters(mood.constraints) else {
            throw ContractError.invalid("The selected mood changed or was removed. Reload the DJ moods.")
        }
    }
}
