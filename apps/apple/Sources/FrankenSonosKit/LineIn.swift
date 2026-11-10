import Foundation

/// One physical input, including the hidden player of a bonded room. An
/// advertised input does not establish that its cable or audio signal is live.
public struct LineInSource: Codable, Equatable, Identifiable, Sendable {
    public let player: String
    public let room: String
    public let name: String
    public let icon: String
    public var id: String { player }
    public var displayName: String { name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty ? "Line-in" : name }

    public func validated() throws -> Self {
        guard Self.validPlayer(player), !room.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              [room, name, icon].allSatisfy({ $0.utf8.count <= 16_384 }) else { throw ClientError.unreadable }
        return self
    }

    static func validPlayer(_ value: String) -> Bool {
        !value.isEmpty && value.utf8.count <= 128 && value.utf8.allSatisfy {
            (48...57).contains($0) || (65...90).contains($0) || (97...122).contains($0) || $0 == 45 || $0 == 95
        }
    }

    func sameSelection(as other: Self) -> Bool {
        player.utf8.elementsEqual(other.player.utf8) && room.utf8.elementsEqual(other.room.utf8)
            && name.utf8.elementsEqual(other.name.utf8) && icon.utf8.elementsEqual(other.icon.utf8)
    }
}

public struct LineInUnavailable: Codable, Equatable, Identifiable, Sendable {
    public let player: String
    public let room: String
    public let detail: String
    public var id: String { player }

    func validated() throws -> Self {
        guard LineInSource.validPlayer(player), !room.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              !detail.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              [room, detail].allSatisfy({ $0.utf8.count <= 16_384 }) else { throw ClientError.unreadable }
        return self
    }
}

/// A successful empty catalog differs from players whose capability could not
/// be read. Both arrays are required; a partial or malformed reply is not empty.
public struct LineInCatalog: Codable, Equatable, Sendable {
    public let sources: [LineInSource]
    public let unavailable: [LineInUnavailable]

    public func validated() throws -> Self {
        var players = Set<String>()
        for source in sources {
            _ = try source.validated()
            guard players.insert(source.player.lowercased()).inserted else { throw ClientError.unreadable }
        }
        for player in unavailable {
            _ = try player.validated()
            guard players.insert(player.player.lowercased()).inserted else { throw ClientError.unreadable }
        }
        return self
    }
}

/// Retains the exact input and destination from the displayed household. A room
/// label never substitutes for a physical player when confirming the choice.
public struct LineInSelection: Equatable, Sendable {
    public let source: LineInSource
    public let zone: Zone

    public init(_ source: LineInSource, in zone: Zone) throws {
        self.source = try source.validated(); self.zone = try zone.validated()
    }

    public var title: String { "Play now: \(source.displayName)" }
    public var explanation: String {
        "Plays the physical input on \(source.room) in \(zone.household) through this group. Input player: \(source.player). " +
            "Keeps the group’s queue and room membership. Playback volume caps apply. " +
            "Cable connection and audio signal are unknown; connect and start your source before playing."
    }

    func fields(in current: Zone) throws -> [String: JSONValue] {
        _ = try current.validated()
        guard zone.sameGroup(as: current) else {
            throw ContractError.invalid("The input’s destination group changed. Choose it again in the intended household.")
        }
        return ["zone": .string(zone.id.address), "source": .string(source.player)]
    }

    func validate(_ current: LineInCatalog, in zone: Zone) throws {
        _ = try fields(in: zone)
        _ = try current.validated()
        let matches = current.sources.filter { $0.player.utf8.elementsEqual(source.player.utf8) }
        guard matches.count == 1, let found = matches.first, source.sameSelection(as: found) else {
            throw ContractError.invalid("The selected input changed, disappeared or could not be read. Refresh line-in and review it again.")
        }
    }
}

extension DaemonClient {
    public func lineInSources(_ zone: Zone) async throws -> LineInCatalog {
        _ = try zone.validated()
        try Task.checkCancellation()
        let catalog = try await get(LineInCatalog.self, ["line-in"],
            query: [URLQueryItem(name: "zone", value: zone.id.address)], timeout: 120)
        try Task.checkCancellation()
        return try catalog.validated()
    }
}
