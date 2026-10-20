import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif

/// Version 1 of the existing daemon doctor report. A skipped prerequisite is
/// not a pass, and receipt of a report is not a physical listening test.
public struct DiagnosticReport: Decodable, Equatable, Sendable {
    public enum Status: String, Codable, CaseIterable, Sendable { case pass, warn, fail, skip }
    public struct Check: Decodable, Equatable, Sendable, Identifiable {
        public let id: String
        public let title: String
        public let status: Status
        public let summary: String
        public let detail: String?
        public let remedy: String?
        public let evidence: JSONValue?
        public let durationMs: UInt64
        public var evidenceText: String? {
            guard let evidence, evidence != .null else { return nil }
            let encoder = JSONEncoder(); encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
            guard let bytes = try? encoder.encode(evidence) else { return nil }
            return String(decoding: bytes, as: UTF8.self)
        }
    }
    public let schema: UInt32
    public let exitCode: Int
    public let counts: [String: UInt]
    public let checks: [Check]
    public static let maximumBytes = 2 * 1_024 * 1_024

    public func validated() throws -> Self {
        func text(_ value: String, maximum: Int = 65_536, nonempty: Bool = false) -> Bool {
            value.utf8.count <= maximum && (!nonempty || !value.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
        }
        guard schema == 1, checks.count <= 2_048,
              Set(counts.keys) == Set(Status.allCases.map(\.rawValue)) else {
            throw ContractError.invalid("The diagnostic report uses an unsupported or incomplete format.")
        }
        var expected = Dictionary(uniqueKeysWithValues: Status.allCases.map { ($0.rawValue, UInt(0)) })
        var seen = Set<String>(), size = 0
        for row in checks {
            guard text(row.id, maximum: 1_024, nonempty: true), seen.insert(row.id).inserted,
                  text(row.title, nonempty: true), text(row.summary),
                  row.detail.map({ text($0) }) ?? true,
                  row.remedy.map({ text($0) }) ?? true else {
                throw ContractError.invalid("A diagnostic check is incomplete, too large, or has a repeated identity.")
            }
            expected[row.status.rawValue, default: 0] += 1
            size += row.id.utf8.count + row.title.utf8.count + row.summary.utf8.count
                + (row.detail?.utf8.count ?? 0) + (row.remedy?.utf8.count ?? 0)
            if let evidence = row.evidence {
                size += try JSONEncoder().encode(evidence).count
            }
            guard size <= Self.maximumBytes else {
                throw ContractError.invalid("The diagnostic report exceeds the display limit.")
            }
        }
        let expectedExit = expected["fail", default: 0] > 0 ? 7 : (expected["warn", default: 0] > 0 ? 6 : 0)
        guard counts == expected, exitCode == expectedExit else {
            throw ContractError.invalid("The diagnostic totals disagree with the individual checks.")
        }
        return self
    }

    public enum Filter: String, CaseIterable, Sendable { case attention, all, fail, warn, skip, pass }
    /// Filtering preserves the daemon's prerequisite order and never reads the network.
    public func visible(filter: Filter = .attention, query: String = "") -> [Check] {
        let needle = query.trimmingCharacters(in: .whitespacesAndNewlines)
        return checks.filter { row in
            let statusMatches = filter == .all || (filter == .attention && row.status != .pass) || filter.rawValue == row.status.rawValue
            return statusMatches && (needle.isEmpty || [row.id, row.title, row.summary, row.detail ?? "", row.remedy ?? ""].contains {
                $0.localizedCaseInsensitiveContains(needle)
            })
        }
    }
}

/// Reuse the existing client and transport, but admit only this fixed GET.
/// The ordinary URLSession transport retains its redirect refusal, ephemeral
/// session and 4 MiB network cap. Diagnostics has a stricter 2 MiB decode cap.
private struct DiagnosticTransport: DaemonTransport {
    let origin: DaemonEndpoint
    let base: any DaemonTransport
    func send(_ request: URLRequest) async throws -> HTTPReply {
        guard request.url == (try origin.resource(["doctor"])),
              (request.httpMethod ?? "GET") == "GET", request.httpBody == nil else {
            throw ContractError.invalid("Diagnostics accepts only a read of the configured daemon's doctor endpoint.")
        }
        var request = request
        request.timeoutInterval = 120
        try Task.checkCancellation()
        let reply = try await base.send(request)
        try Task.checkCancellation()
        guard reply.data.count <= DiagnosticReport.maximumBytes else {
            throw ContractError.invalid("The diagnostic response exceeds the 2 MiB limit.")
        }
        return reply
    }
    func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> {
        AsyncThrowingStream { $0.finish(throwing: ContractError.invalid("Diagnostics does not subscribe to events.")) }
    }
}

/// On-demand diagnostics, independent of playback selection. Opening,
/// foregrounding and reconnecting never launch an expensive report or a fix.
@MainActor public final class DiagnosticsModel {
    public var onChange: (@MainActor () -> Void)?
    public private(set) var active = false { willSet { onChange?() } }
    public private(set) var running = false { willSet { onChange?() } }
    public private(set) var report: DiagnosticReport? { willSet { onChange?() } }
    public private(set) var receivedAt: Date? { willSet { onChange?() } }
    public private(set) var stale = false { willSet { onChange?() } }
    public private(set) var problem: String? { willSet { onChange?() } }
    public var canRun: Bool { active && !stopped && !running }
    private let client: DaemonClient
    private let deadline: Duration
    private var generation: UInt64 = 0
    private var stopped = false
    private var worker: Task<Void, Never>?
    private var timer: Task<Void, Never>?

    public init(endpoint: DaemonEndpoint, transport: any DaemonTransport = URLSessionTransport(),
                deadline: Duration = .seconds(120)) {
        client = DaemonClient(endpoint: endpoint, transport: DiagnosticTransport(origin: endpoint, base: transport))
        self.deadline = min(max(deadline, .milliseconds(1)), .seconds(120))
    }

    public func resume() { guard !stopped else { return }; active = true }
    public func suspend() {
        active = false
        cancel()
    }
    public func stop() { suspend(); stopped = true }

    public func run() {
        guard canRun else { return }
        generation &+= 1
        let token = generation, client = client, deadline = deadline
        running = true; stale = report != nil; problem = nil
        worker = Task { [weak self] in
            do {
                let report = try await client.get(DiagnosticReport.self, ["doctor"]).validated()
                try Task.checkCancellation()
                if let self, self.active && self.generation == token && !self.stopped {
                    self.report = report; self.receivedAt = Date(); self.stale = false
                }
            } catch {
                if let self, self.active && self.generation == token && !Task.isCancelled && !self.stopped {
                    self.problem = error.localizedDescription
                }
            }
            self?.finish()
        }
        timer = Task { [weak self] in
            do { try await Task.sleep(for: deadline) } catch { return }
            guard let self, self.running && self.generation == token else { return }
            self.generation &+= 1
            self.problem = "No complete report arrived before the deadline. Checks on the daemon may still finish; no fix or playback command was sent."
            self.worker?.cancel()
            // A transport ignoring cancellation retains the worker slot until
            // it drains. Neither a second click nor a reopen starts a duplicate.
        }
    }

    public func cancel() {
        generation &+= 1; stale = report != nil
        timer?.cancel(); timer = nil
        if running {
            worker?.cancel()
            problem = "The diagnostic request was cancelled. Checks on the daemon may still finish; no fix or playback command was sent."
        }
    }
    private func finish() {
        timer?.cancel(); timer = nil; worker = nil; running = false
    }
    deinit { worker?.cancel(); timer?.cancel() }
}
