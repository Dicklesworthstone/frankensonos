import Foundation

/// The versioned GET /doctor report. A successful HTTP read can still contain
/// failed checks; skipped checks never establish that a capability works.
public struct DiagnosticsReport: Decodable, Equatable, Sendable {
    public static let requestTimeout: TimeInterval = 180
    public let schema: UInt32
    public let exitCode: UInt8
    public let counts: Counts
    public let checks: [Check]

    public enum Status: String, Decodable, CaseIterable, Sendable {
        case pass, warn, fail, skip
        public var title: String {
            switch self { case .pass: "Passed"; case .warn: "Warning"; case .fail: "Failed"; case .skip: "Skipped" }
        }
        public var needsAttention: Bool { self == .fail || self == .warn }
    }
    public enum Filter: String, CaseIterable, Sendable {
        case all, attention, skipped
        public var title: String {
            switch self { case .all: "All checks"; case .attention: "Needs attention"; case .skipped: "Skipped" }
        }
    }
    public struct Counts: Decodable, Equatable, Sendable {
        public let pass: UInt32
        public let warn: UInt32
        public let fail: UInt32
        public let skip: UInt32
    }
    public struct Check: Decodable, Equatable, Sendable, Identifiable {
        public let id: String
        public let title: String
        public let status: Status
        public let summary: String
        public let detail: String?
        public let remedy: String?
        public let evidence: JSONValue?
        public let durationMs: UInt64

        /// Evidence is inert text, not a link, command, or a second request.
        public var evidenceText: String? {
            guard let evidence, evidence != .null else { return nil }
            let encoder = JSONEncoder()
            encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
            guard let bytes = try? encoder.encode(evidence) else { return nil }
            return String(data: bytes, encoding: .utf8)
        }
    }

    public func validated() throws -> Self {
        guard schema == 1 else {
            throw ContractError.invalid("This diagnostic report uses an unsupported schema. Update the remote before interpreting it.")
        }
        func text(_ value: String, maximum: Int) -> Bool {
            !value.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
                && value.utf8.count <= maximum && !value.contains("\0")
        }
        guard checks.count <= 2_048, Set(checks.map(\.id)).count == checks.count,
              checks.allSatisfy({ check in
                  text(check.id, maximum: 512) && text(check.title, maximum: 4_096)
                      && text(check.summary, maximum: 65_536)
                      && [check.detail, check.remedy].allSatisfy { value in
                          value.map { $0.utf8.count <= 262_144 && !$0.contains("\0") } ?? true
                      }
              }) else {
            throw ContractError.invalid("The diagnostic report has incomplete or duplicate checks.")
        }
        let actual = Counts(pass: UInt32(checks.filter { $0.status == .pass }.count),
                            warn: UInt32(checks.filter { $0.status == .warn }.count),
                            fail: UInt32(checks.filter { $0.status == .fail }.count),
                            skip: UInt32(checks.filter { $0.status == .skip }.count))
        let expectedExit: UInt8 = actual.fail > 0 ? 7 : actual.warn > 0 ? 6 : 0
        guard counts == actual, exitCode == expectedExit else {
            throw ContractError.invalid("The diagnostic report's totals contradict its checks. Run diagnostics again.")
        }
        return self
    }

    public var headline: String {
        if checks.isEmpty { return "No checks reported" }
        if counts.fail > 0 { return "Setup problems found" }
        if counts.warn > 0 { return "Setup needs attention" }
        if counts.pass == 0 { return "No checks ran" }
        return counts.skip > 0 ? "Completed checks passed" : "Reported checks passed"
    }
    public var summary: String {
        "\(counts.pass) passed · \(counts.warn) warnings · \(counts.fail) failed · \(counts.skip) skipped"
    }
    public func filtered(_ filter: Filter, query: String = "") -> [Check] {
        let query = query.trimmingCharacters(in: .whitespacesAndNewlines)
        return checks.filter { check in
            let matches = filter == .all || (filter == .attention && check.status.needsAttention)
                || (filter == .skipped && check.status == .skip)
            return matches && (query.isEmpty || [check.id, check.title, check.summary, check.detail ?? "", check.remedy ?? ""]
                .contains { $0.localizedCaseInsensitiveContains(query) })
        }
    }

    /// Explicit user export only. Even without machine evidence, summaries or
    /// remedies can contain private room names, addresses and local paths.
    public func text(includeEvidence: Bool = false) -> String {
        var lines = ["FrankenSonos · Setup diagnostics", headline, summary,
                     "Read-only daemon report. Skipped checks were not verified. No repairs were performed."]
        for check in checks {
            lines.append("\n\(check.status.title) · \(check.id) · \(check.title)")
            lines.append(check.summary)
            if let detail = check.detail, !detail.isEmpty { lines.append(detail) }
            if let remedy = check.remedy, !remedy.isEmpty { lines.append("Suggested remedy: " + remedy) }
            lines.append("Check duration: \(check.durationMs) ms")
            if includeEvidence, let evidence = check.evidenceText { lines.append("Evidence:\n" + evidence) }
        }
        return lines.joined(separator: "\n")
    }
}

public struct DiagnosticsObservation: Sendable, Identifiable {
    public let id = UUID()
    public let report: DiagnosticsReport
    /// Time received by this client; the API may reuse a recent daemon report.
    public let receivedAt: Date
}
