import Foundation

/// Reusable steering for a scene or scheduled start. Relative time begins
/// with each start. Keep every field for immutable-definition comparisons;
/// the existing steering vocabulary validates and describes the filters.
/// Canonical defaults make a returned energy_bias: 0 equivalent to omission.
public struct DJDirection: Codable, Equatable, Sendable {
    public let constraints: [String: JSONValue]
    public let forSecs: UInt32?

    public init(constraints: [String: JSONValue] = [:], forSecs: UInt32? = nil) {
        self.constraints = constraints; self.forSecs = forSecs
    }

    public init(from decoder: Decoder) throws {
        let fields = try decoder.singleValueContainer().decode([String: JSONValue].self)
        guard Set(fields.keys).isSubset(of: ["constraints", "for_secs"]) else {
            throw ContractError.invalid("This saved DJ direction has unsupported fields. Use a compatible client before applying it.")
        }
        if let raw = fields["constraints"] {
            guard case .object(let filters) = raw else { throw ContractError.invalid("The saved DJ filters are incomplete.") }
            constraints = try DJSteeringFilters(filters).values
        } else { constraints = [:] }
        switch fields["for_secs"] {
        case nil, .null?: forSecs = nil
        case .integer(let seconds)?:
            guard (1...604_800).contains(seconds) else {
                throw ContractError.invalid("A saved DJ direction lasts from one second to one week, or until cleared.")
            }
            forSecs = UInt32(seconds)
        default: throw ContractError.invalid("The saved DJ duration must be whole seconds.")
        }
    }

    public func encode(to encoder: Encoder) throws {
        var value = encoder.singleValueContainer()
        try value.encode(fields)
    }

    var fields: [String: JSONValue] {
        var result: [String: JSONValue] = ["constraints": .object(constraints)]
        if let forSecs { result["for_secs"] = .integer(Int64(forSecs)) }
        return result
    }

    public func validated(mood: String?) throws -> Self {
        let filters = try DJSteeringFilters(constraints)
        if let mood { _ = try DJSteeringFilters.word(mood) }
        guard forSecs.map({ (1...604_800).contains($0) }) ?? true else {
            throw ContractError.invalid("A saved DJ direction lasts from one second to one week, or until cleared.")
        }
        guard forSecs == nil || mood != nil || !filters.values.isEmpty else {
            throw ContractError.invalid("Choose a mood or filter before giving a saved DJ direction a duration.")
        }
        return Self(constraints: filters.values, forSecs: forSecs)
    }

    public func summary(mood: String?) -> String {
        guard let filters = try? DJSteeringFilters(constraints) else { return "Saved DJ filters are unreadable." }
        if mood == nil && filters.values.isEmpty {
            return "Return to the automatic program before starting; clear temporary steering."
        }
        let lifetime = forSecs.map { "\($0) seconds from each start" } ?? "until replaced or cleared"
        return "DJ mood: \(mood ?? "Automatic program")\n\(filters.summary)\nDuration: \(lifetime)."
    }
}

extension Scene.Source {
    public var djSummary: String {
        direction?.summary(mood: mood) ?? mood.map { "DJ mood: " + $0 } ?? "Start the DJ with its existing direction"
    }
}
