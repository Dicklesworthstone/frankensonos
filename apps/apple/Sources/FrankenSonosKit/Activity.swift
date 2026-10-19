import Foundation

/// A daemon action-log record. `undoable` describes the saved before-state,
/// not authorization or a promise that this particular row can be undone now.
public struct LoggedAction: Codable, Equatable, Sendable, Identifiable {
    public let id: Int64
    public let at: Int64
    public let client: String
    public let surface: String
    public let intent: String
    public let decision: String
    public let result: String
    public let undoable: Bool
    public let undoOf: Int64?
    public var date: Date { Date(timeIntervalSince1970: Double(at)) }

    func validate() throws {
        guard id > 0, ActivityQuery.validTimestamp(at), undoOf.map({ $0 > 0 }) ?? true,
              !(undoable && undoOf != nil),
              [client, surface, intent, decision, result].allSatisfy({ $0.utf8.count <= 65_536 }) else {
            throw ContractError.invalid("The action log contains an incomplete or invalid record.")
        }
    }
}

/// The history endpoint has no persistent row ID or household ID. Do not
/// deduplicate these values: repeated plays can have identical wire records.
public struct RecordedPlay: Codable, Equatable, Sendable {
    public let room: String?
    public let sourceUri: String
    public let title: String?
    public let artist: String?
    public let playedAt: Int64
    public var date: Date { Date(timeIntervalSince1970: Double(playedAt)) }
    public var displayTitle: String { title.flatMap { $0.isEmpty ? nil : $0 } ?? "Recorded source" }

    func validate() throws {
        guard ActivityQuery.validTimestamp(playedAt), !sourceUri.isEmpty, sourceUri.utf8.count <= 4_096,
              [room, title, artist].compactMap({ $0 }).allSatisfy({ $0.utf8.count <= 65_536 }) else {
            throw ContractError.invalid("The listening history contains an incomplete or invalid record.")
        }
    }

    /// Only portable sources can be offered to another current group. Raw
    /// renderer URIs embed service/queue context and are never reinterpreted.
    public var replaySource: String? {
        let value = sourceUri
        guard value == value.trimmingCharacters(in: .whitespacesAndNewlines),
              !value.unicodeScalars.contains(where: CharacterSet.controlCharacters.contains) else { return nil }
        let parts = value.split(separator: ":", omittingEmptySubsequences: false)
        if parts.count == 3, parts[0] == "spotify", ["track", "album", "playlist"].contains(parts[1]),
           parts[2].utf8.count == 22,
           parts[2].utf8.allSatisfy({ (48...57).contains($0) || (65...90).contains($0) || (97...122).contains($0) }) {
            return value
        }
        guard let url = URLComponents(string: value), let host = url.host, !host.isEmpty,
              ["http", "https"].contains(url.scheme?.lowercased() ?? ""),
              url.user == nil, url.password == nil, url.fragment == nil,
              url.port.map({ (1...65_535).contains($0) }) ?? true else { return nil }
        return value
    }
}

/// Filters for two independent store reads. `client` and `since` apply only
/// to the action log; `historyRoom` applies only to listening history.
public struct ActivityQuery: Equatable, Sendable {
    public var client: String?
    public var since: Int64?
    public var historyRoom: RoomID?
    public var limit: Int
    public init(client: String? = nil, since: Int64? = nil, historyRoom: RoomID? = nil, limit: Int = 50) {
        self.client = client; self.since = since; self.historyRoom = historyRoom; self.limit = limit
    }
    static func validTimestamp(_ value: Int64) -> Bool { (-62_135_596_800...253_402_300_799).contains(value) }
    public func validated() throws -> Self {
        guard (1...200).contains(limit), since.map(Self.validTimestamp) ?? true else {
            throw ContractError.invalid("Choose 1–200 recent records and a valid action-log start time.")
        }
        func validName(_ value: String) -> Bool {
            !value.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty && value.utf8.count <= 4_096 &&
            !value.unicodeScalars.contains(where: CharacterSet.controlCharacters.contains)
        }
        guard client.map(validName) ?? true, historyRoom.map({ validName($0.room) && validName($0.household) }) ?? true else {
            throw ContractError.invalid("Use an exact client identity or a household-qualified history room.")
        }
        return self
    }
    var actionItems: [URLQueryItem] {
        var result = [URLQueryItem(name: "limit", value: String(limit))]
        if let client { result.append(URLQueryItem(name: "client", value: client)) }
        if let since { result.append(URLQueryItem(name: "since", value: String(since))) }
        return result
    }
    var historyItems: [URLQueryItem] {
        var result = [URLQueryItem(name: "limit", value: String(limit))]
        if let historyRoom { result.append(URLQueryItem(name: "zone", value: historyRoom.address)) }
        return result
    }
}

extension DaemonClient {
    public func recentActions(_ query: ActivityQuery = ActivityQuery()) async throws -> [LoggedAction] {
        _ = try query.validated()
        let records = try await get([LoggedAction].self, ["actions"], query: query.actionItems)
        guard records.count <= query.limit, Set(records.map(\.id)).count == records.count else {
            throw ContractError.invalid("The action log is oversized or has repeated record identities.")
        }
        for record in records {
            try record.validate()
            guard query.client.map({ $0 == record.client }) ?? true,
                  query.since.map({ record.at >= $0 }) ?? true else {
                throw ContractError.invalid("The action log does not match the requested filter.")
            }
        }
        return records
    }
    public func recentPlays(_ query: ActivityQuery = ActivityQuery()) async throws -> [RecordedPlay] {
        _ = try query.validated()
        let records = try await get([RecordedPlay].self, ["history"], query: query.historyItems)
        guard records.count <= query.limit else { throw ContractError.invalid("The listening history exceeds the requested limit.") }
        for record in records { try record.validate() }
        // Room labels can be absent, or name a coordinator rather than the
        // requested member. They are not evidence of a household mismatch.
        return records
    }
}
