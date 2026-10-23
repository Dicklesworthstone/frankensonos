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
            guard source.kind == "queue" || source.kind == "queue_backup" || source.queue == nil else {
                throw ContractError.invalid("A scene has a queue reference attached to another music source. Capture it again.")
            }
            guard source.kind == "dj" || source.direction == nil else {
                throw ContractError.invalid("A saved DJ direction is attached to another scene music source.")
            }
            switch source.kind {
            case "keep": break
            case "dj":
                _ = try source.direction?.validated(mood: source.mood)
            case "favorite":
                guard source.name.map(named) == true else { throw ContractError.invalid("The scene favorite has no name.") }
            case "uri":
                guard source.uri.map(named) == true else { throw ContractError.invalid("The scene source has no URI.") }
            case "queue", "queue_backup":
                func identity(_ value: String) -> Bool {
                    named(value) && value.utf8.count <= 1_024 &&
                        !value.unicodeScalars.contains(where: CharacterSet.controlCharacters.contains)
                }
                guard let queue = source.queue, identity(queue.coordinator), queue.track > 0,
                      queue.household.map(identity) != false,
                      source.name == nil, source.uri == nil, source.mood == nil else {
                    throw ContractError.invalid("The scene's saved queue position is incomplete. Capture the house again.")
                }
                if source.kind == "queue_backup" {
                    guard let total = queue.total, (1...1_000).contains(total), queue.track <= total else {
                        throw ContractError.invalid("The scene's saved queue backup is incomplete or exceeds the 1,000-entry limit. Capture the house again.")
                    }
                } else if queue.total != nil {
                    throw ContractError.invalid("A queue bookmark cannot contain a backup entry count. Refresh scenes.")
                }
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
        guard scenes.count <= 256, Set(scenes.map { SceneName.key($0.name) }).count == scenes.count else {
            throw ContractError.invalid("The scene list is ambiguous or too large. Refresh scenes.")
        }
        return try scenes.map { try $0.validated() }
    }

    public func scene(named name: String) async throws -> Scene {
        let scene = try await get(Scene.self, ["scenes", name]).validated()
        guard scene.name.utf8.elementsEqual(name.utf8) else { throw ContractError.invalid("The daemon returned a different saved scene.") }
        return scene
    }
}
