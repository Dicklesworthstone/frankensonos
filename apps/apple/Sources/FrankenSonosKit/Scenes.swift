import Foundation

extension Scene {
    /// Validate the definition that will be displayed and reviewed. Keep its
    /// room references verbatim; only the daemon resolves scoped names/IDs.
    public func validated() throws -> Self {
        func named(_ value: String) -> Bool {
            !value.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty && value.utf8.count <= 4_096
        }
        guard named(name), groups.count <= 256, volumes.count <= 1_024, mutes.count <= 1_024,
              volumes.allSatisfy({ named($0.key) && (0...100).contains($0.value) }),
              mutes.keys.allSatisfy(named),
              groups.reduce(0, { $0 + $1.members.count + 1 }) <= 1_024 else {
            throw ContractError.invalid("The saved scene is incomplete or exceeds the house limits. Refresh scenes.")
        }
        for group in groups {
            let rooms = [group.coordinator] + group.members
            guard rooms.allSatisfy(named), Set(rooms).count == rooms.count else {
                throw ContractError.invalid("A saved scene group has missing or repeated rooms.")
            }
            let source = group.source
            switch source.kind {
            case "keep", "dj": break
            case "favorite":
                guard source.name.map(named) == true else { throw ContractError.invalid("The scene favorite has no name.") }
            case "uri":
                guard source.uri.map(named) == true else { throw ContractError.invalid("The scene source has no URI.") }
            default:
                throw ContractError.invalid("The scene uses an unsupported source: \(source.kind).")
            }
        }
        return self
    }
}

extension DaemonClient {
    public func scenes() async throws -> [Scene] {
        let scenes = try await get([Scene].self, ["scenes"])
        guard scenes.count <= 256, Set(scenes.map(\.name)).count == scenes.count else {
            throw ContractError.invalid("The scene list is ambiguous or too large. Refresh scenes.")
        }
        return try scenes.map { try $0.validated() }
    }

    public func scene(named name: String) async throws -> Scene {
        let scene = try await get(Scene.self, ["scenes", name]).validated()
        guard scene.name == name else { throw ContractError.invalid("The daemon returned a different saved scene.") }
        return scene
    }
}
