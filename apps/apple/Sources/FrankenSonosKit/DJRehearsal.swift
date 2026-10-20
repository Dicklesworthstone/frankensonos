import Foundation

/// Preview-only choices. A rehearsal never starts, queues, steers or records music.
public struct DJRehearsalDraft: Equatable, Sendable {
    public enum Length: String, CaseIterable, Sendable { case works, hours }
    public enum Start: String, CaseIterable, Sendable { case now, houseTime }
    public enum Filter: String, CaseIterable, Sendable {
        case includeGenres = "include_genres", excludeGenres = "exclude_genres"
        case includeArtists = "include_artists", excludeArtists = "exclude_artists"
        case includeComposers = "include_composers", excludeComposers = "exclude_composers"
        case includeKeywords = "include_keywords", excludeKeywords = "exclude_keywords"
        case periods, excludePeriods = "exclude_periods", decades, excludeDecades = "exclude_decades"
        public var title: String {
            switch self {
            case .includeGenres: "Include genres"
            case .excludeGenres: "Exclude genres"
            case .includeArtists: "Include artists or performers"
            case .excludeArtists: "Exclude artists or performers"
            case .includeComposers: "Include composers"
            case .excludeComposers: "Exclude composers"
            case .includeKeywords: "Include instruments or keywords"
            case .excludeKeywords: "Exclude instruments or keywords"
            case .periods: "Include classical periods"
            case .excludePeriods: "Exclude classical periods"
            case .decades: "Include release decades"
            case .excludeDecades: "Exclude release decades"
            }
        }
    }
    public var length: Length = .works
    public var works = 12
    public var hours = 2
    public var start: Start = .now
    public var houseTime = "23:00"
    public var mood = ""
    /// Decimal text preserves the complete UInt64 range, including values above 2^53.
    public var seed = "1"
    public var energyBias = 0
    public var allowLongWorks = false
    public var minimumMinutes = ""
    public var maximumMinutes = ""
    /// One literal value per line. Commas are part of names, never separators.
    public var filters: [Filter: String] = [:]
    public init() {}

    public func request(for zone: Zone) throws -> DJRehearsalRequest {
        _ = try zone.validated()
        guard length == .works ? (1...500).contains(works) : (1...48).contains(hours) else {
            throw ContractError.invalid("Choose 1–500 works or 1–48 hours.")
        }
        guard let seed = Self.unsigned(seed, as: UInt64.self) else {
            throw ContractError.invalid("The variation seed must be a whole number from 0 to 18446744073709551615.")
        }
        let time = houseTime.trimmingCharacters(in: .whitespacesAndNewlines)
        if start == .houseTime {
            let bytes = Array(time.utf8)
            guard bytes.count == 5, bytes[2] == 58,
                  [bytes[0], bytes[1], bytes[3], bytes[4]].allSatisfy({ (48...57).contains($0) }),
                  let hour = Int(time.prefix(2)), hour < 24,
                  let minute = Int(time.suffix(2)), minute < 60 else {
                throw ContractError.invalid("Enter a house-clock time as HH:MM, from 00:00 to 23:59.")
            }
        }
        let mood = mood.trimmingCharacters(in: .whitespacesAndNewlines)
        guard mood.unicodeScalars.count <= 256, !mood.contains(where: { $0.isNewline }) else {
            throw ContractError.invalid("Choose one mood name of at most 256 characters.")
        }
        guard (-2...2).contains(energyBias) else { throw ContractError.invalid("Energy bias must be between −2 and +2.") }
        var constraints: [String: JSONValue] = [:]
        if energyBias != 0 { constraints["energy_bias"] = .integer(Int64(energyBias)) }
        if allowLongWorks { constraints["allow_long_works"] = .bool(true) }
        func minutes(_ raw: String) throws -> UInt32? {
            if raw.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty { return nil }
            guard let value = Self.unsigned(raw, as: UInt32.self) else {
                throw ContractError.invalid("Work lengths must be non-negative whole minutes, or left blank.")
            }
            return value
        }
        let minimum = try minutes(minimumMinutes), maximum = try minutes(maximumMinutes)
        if let minimum, let maximum, minimum > maximum {
            throw ContractError.invalid("The minimum work length must not exceed the maximum.")
        }
        if let minimum { constraints["min_work_minutes"] = .integer(Int64(minimum)) }
        if let maximum { constraints["max_work_minutes"] = .integer(Int64(maximum)) }
        for filter in Filter.allCases {
            let raw = filters[filter] ?? ""
            // Bound the native text editor before splitting an untrusted paste.
            guard raw.utf8.count <= 65_536 else { throw ContractError.invalid("Keep each filter under 64 KiB.") }
            let lines = raw.components(separatedBy: .newlines)
                .map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }.filter { !$0.isEmpty }
            guard lines.count <= 64, lines.allSatisfy({ $0.unicodeScalars.count <= 256 }) else {
                throw ContractError.invalid("Use at most 64 entries per filter, each at most 256 characters.")
            }
            guard !lines.isEmpty else { continue }
            let values: [JSONValue]
            switch filter {
            case .decades, .excludeDecades:
                values = try lines.map { line in
                    guard let value = Self.unsigned(line, as: UInt16.self), value.isMultiple(of: 10) else {
                        throw ContractError.invalid("Name each release decade by its first year, such as 1960 or 1990.")
                    }
                    return .integer(Int64(value))
                }
            case .periods, .excludePeriods:
                values = try lines.map { line in
                    let value = line.lowercased().replacingOccurrences(of: " ", with: "_")
                    guard Self.periods.contains(value) else {
                        throw ContractError.invalid("Unknown classical period: \(line). Choose a supported period.")
                    }
                    return .string(value)
                }
            default: values = lines.map(JSONValue.string)
            }
            constraints[filter.rawValue] = .array(values)
        }
        return DJRehearsalRequest(zone: zone, works: length == .works ? UInt32(works) : nil,
                                  hours: length == .hours ? UInt32(hours) : nil,
                                  at: start == .houseTime ? time : nil, mood: mood.isEmpty ? nil : mood,
                                  seed: seed, constraints: constraints)
    }

    public static let periods = ["medieval", "renaissance", "baroque", "classical", "romantic",
                                 "late_romantic", "impressionist", "modern", "contemporary"]
    private static func unsigned<T: FixedWidthInteger & UnsignedInteger>(_ raw: String, as: T.Type) -> T? {
        let value = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !value.isEmpty, value.utf8.allSatisfy({ (48...57).contains($0) }) else { return nil }
        return T(value)
    }
}

/// Immutable input and destination captured at Generate, not whatever is selected later.
public struct DJRehearsalRequest: Equatable, Sendable, Encodable {
    public let zone: Zone
    public let works: UInt32?
    public let hours: UInt32?
    public let at: String?
    public let mood: String?
    public let seed: UInt64
    public let constraints: [String: JSONValue]
    fileprivate init(zone: Zone, works: UInt32?, hours: UInt32?, at: String?, mood: String?,
                     seed: UInt64, constraints: [String: JSONValue]) {
        self.zone = zone; self.works = works; self.hours = hours; self.at = at
        self.mood = mood; self.seed = seed; self.constraints = constraints
    }
    public var overridesSteering: Bool { mood != nil || !constraints.isEmpty }
    public var scopeDescription: String {
        overridesSteering ? "Preview-only mood and filters replace this group’s steering. Standing taste still applies."
            : "Uses this group’s steering, including its expiry and the house’s time-of-day program."
    }
    private enum CodingKeys: String, CodingKey { case zone, works, hours, at, mood, seed, constraints }
    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(zone.id.address, forKey: .zone)
        try c.encodeIfPresent(works, forKey: .works); try c.encodeIfPresent(hours, forKey: .hours)
        try c.encodeIfPresent(at, forKey: .at); try c.encodeIfPresent(mood, forKey: .mood)
        try c.encode(seed, forKey: .seed)
        if !constraints.isEmpty { try c.encode(constraints, forKey: .constraints) }
    }
}

/// The daemon's rehearsal DTO. This is a hypothetical set, not a queue or a reservation.
public struct DJRehearsal: Decodable, Equatable, Sendable {
    public let zone: String
    public let from: String
    public let seed: UInt64
    public let works: [Pick]
    public let summary: Summary
    public struct Pick: Decodable, Equatable, Sendable {
        public let at: String
        public let work: DJWork
        public let genre: String?
        public let energy: UInt8
        public let energyTarget: UInt8?
        public let mood: String?
        public let explicit: Bool
    }
    public struct Summary: Decodable, Equatable, Sendable {
        public let works: UInt32
        public let minutes: UInt32
        public let distinctArtists: UInt32
        public let explicit: UInt32
        public let hours: [Hour]
    }
    public struct Hour: Decodable, Equatable, Sendable {
        public let from: String
        public let works: UInt32
        public let artistEntropy: Double
        public let genreEntropy: Double
        public let energy: UInt8
        public let energyTarget: UInt8?
    }
    public func validated(for request: DJRehearsalRequest) throws -> Self {
        guard zone == request.zone.coordinatorRoom, seed == request.seed,
              works.count <= Int(request.works ?? 500), summary.works == works.count,
              summary.distinctArtists <= summary.works, summary.explicit == works.filter(\.explicit).count,
              works.isEmpty ? (summary.distinctArtists == 0 && summary.minutes == 0 && summary.hours.isEmpty)
                : (summary.distinctArtists > 0 && !summary.hours.isEmpty),
              let start = Self.timestamp(from), request.at.map({ Self.clock(from) == $0 }) ?? true else {
            throw ContractError.invalid("The rehearsal is incomplete or belongs to different preview choices.")
        }
        var previous = start
        for (index, pick) in works.enumerated() {
            guard let at = Self.timestamp(pick.at), at >= previous, index != 0 || at == start,
                  request.hours.map({ at.timeIntervalSince(start) < Double($0) * 3600 }) ?? true,
                  pick.energy <= 100, pick.energyTarget.map({ $0 <= 100 }) ?? true,
                  pick.work.movements > 0,
                  pick.work.movement.map({ (1...pick.work.movements).contains($0) }) ?? true else {
                throw ContractError.invalid("The rehearsal contains an invalid work, start time or energy level.")
            }
            previous = at
        }
        var offset = 0
        for hour in summary.hours {
            let count = Int(hour.works)
            guard count > 0, count <= works.count - offset,
                  let at = Self.timestamp(hour.from), at == Self.timestamp(works[offset].at),
                  hour.energy <= 100, hour.energyTarget.map({ $0 <= 100 }) ?? true,
                  [hour.artistEntropy, hour.genreEntropy].allSatisfy({
                      $0.isFinite && $0 >= 0 && $0 <= log2(Double(count)) + 0.000_001
                  }) else { throw ContractError.invalid("The rehearsal’s hourly summary is inconsistent.") }
            offset += count
        }
        guard offset == works.count else { throw ContractError.invalid("The rehearsal’s hourly work counts do not match.") }
        return self
    }
    /// Keep the daemon's offset visible; do not silently reinterpret house time as phone time.
    public static func clock(_ timestamp: String) -> String { String(timestamp.dropFirst(11).prefix(5)) }
    public static func day(_ timestamp: String) -> String { String(timestamp.prefix(10)) }
    public static func offset(_ timestamp: String) -> String {
        timestamp.hasSuffix("Z") ? "UTC" : "UTC" + String(timestamp.suffix(6))
    }
    static func timestamp(_ raw: String) -> Date? {
        guard raw.range(of: #"^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(\.[0-9]+)?(Z|[+-][0-9]{2}:[0-9]{2})$"#,
                        options: .regularExpression) != nil else { return nil }
        let parser = ISO8601DateFormatter()
        parser.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        if let date = parser.date(from: raw) { return date }
        parser.formatOptions = [.withInternetDateTime]
        return parser.date(from: raw)
    }
    public var relaxedWorkCount: Int { works.filter { !($0.work.reason.relaxed ?? []).isEmpty }.count }
    public func text(for request: DJRehearsalRequest) -> String {
        var lines = ["FrankenSonos · DJ rehearsal", "Preview only — nothing played, queued or saved.",
                     "Group: \(request.zone.title) · \(request.zone.household)", "Starts: \(from)", "Seed: \(seed)",
                     request.scopeDescription,
                     "\(summary.works) works · \(summary.minutes) planned minutes · \(summary.distinctArtists) artists"]
        for (index, pick) in works.enumerated() {
            lines.append("\n\(index + 1). \(pick.at) · \(pick.work.composer) — \(pick.work.title)")
            lines.append("\(pick.work.movements) movement(s) · energy \(pick.energy)" + (pick.explicit ? " · Explicit" : ""))
            lines.append(pick.work.reason.summary)
            if let relaxed = pick.work.reason.relaxed, !relaxed.isEmpty { lines.append("Relaxed filters: " + relaxed.joined(separator: ", ")) }
        }
        lines.append("\nA new live session may choose differently as library, history, preferences and time change.")
        return lines.joined(separator: "\n")
    }
}
