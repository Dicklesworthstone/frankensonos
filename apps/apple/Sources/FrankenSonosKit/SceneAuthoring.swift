import Foundation

/// Scene names use the daemon's ASCII-only case equivalence. Unicode case
/// folding would accidentally replace a distinct stored name.
public enum SceneName {
    public static func key(_ value: String) -> [UInt8] {
        // String equality also folds Unicode normalization forms, whereas the
        // daemon compares UTF-8 after changing only ASCII A...Z.
        value.utf8.map { (65...90).contains($0) ? $0 + 32 : $0 }
    }

    public static func validated(_ value: String) throws -> String {
        let name = value.trimmingCharacters(in: .whitespacesAndNewlines)
        guard (1...64).contains(name.unicodeScalars.count) else {
            throw ContractError.invalid("Give the scene a name from 1 to 64 characters.")
        }
        return name
    }
}

public enum SceneSourceKind: String, CaseIterable, Hashable, Sendable { case keep, favorite, uri, dj }
public enum ScenePlayback: String, CaseIterable, Sendable { case unchanged, playing, paused }

public struct SceneGroupDraft: Equatable, Sendable, Identifiable {
    public var id = UUID()
    public var coordinator: RoomID?
    public var members: Set<RoomID> = []
    public var source: SceneSourceKind = .keep
    public var value = ""
    public var playback: ScenePlayback = .unchanged
    public init() {}
}

/// Always a newly authored definition. Captured Scene DTOs deliberately do not
/// initialize this form: they omit opaque renderer metadata needed on replay.
public struct SceneDraft: Equatable, Sendable {
    public var name = ""
    public var groups: [SceneGroupDraft] = []
    public var volumes: [RoomID: Int] = [:]
    public var mutes: [RoomID: Bool] = [:]
    public init() {}

    public func definition(zones: [Zone]) throws -> SceneDefinition {
        _ = try SceneName.validated(name)
        let groups = try groups.map { group -> Scene.Group in
            guard let coordinator = group.coordinator else {
                throw ContractError.invalid("Choose the room that leads each scene group.")
            }
            guard group.source == .keep || group.playback != .unchanged else {
                throw ContractError.invalid("Choose Play or Pause for every new music source.")
            }
            let value = group.value.trimmingCharacters(in: .whitespacesAndNewlines)
            let source = Scene.Source(kind: group.source.rawValue,
                name: group.source == .favorite ? value : nil,
                uri: group.source == .uri ? value : nil,
                mood: group.source == .dj && !value.isEmpty ? value : nil)
            return Scene.Group(coordinator: coordinator.address,
                members: group.members.map(\.address).sorted(), source: source,
                playing: group.playback == .unchanged ? nil : group.playback == .playing)
        }
        return try SceneDefinition(groups: groups,
            volumes: Dictionary(uniqueKeysWithValues: volumes.map { ($0.key.address, $0.value) }),
            mutes: Dictionary(uniqueKeysWithValues: mutes.map { ($0.key.address, $0.value) })).validated(zones: zones)
    }
}

/// The PUT body, without DTO-only name/saved fields. Empty bodies mean capture
/// to the daemon, so an empty custom definition is rejected before any I/O.
public struct SceneDefinition: Codable, Equatable, Sendable {
    public let groups: [Scene.Group]
    public let volumes: [String: Int]
    public let mutes: [String: Bool]
    public init(groups: [Scene.Group] = [], volumes: [String: Int] = [:], mutes: [String: Bool] = [:]) {
        self.groups = groups; self.volumes = volumes; self.mutes = mutes
    }

    func validated(zones: [Zone]) throws -> Self {
        guard !groups.isEmpty || !volumes.isEmpty || !mutes.isEmpty else {
            throw ContractError.invalid("Add at least one group, room level or mute setting. An empty custom scene cannot be saved.")
        }
        _ = try Zone.validate(zones)
        let inventory = zones.flatMap { $0.rooms }
        guard Set(inventory.map(\.address)).count == inventory.count else {
            throw ContractError.invalid("Some qualified room addresses are ambiguous. Refresh the house before authoring a scene.")
        }
        let rooms = Dictionary(uniqueKeysWithValues: inventory.map { ($0.address, $0) })
        guard !rooms.isEmpty else { throw ContractError.invalid("Connect to the house before saving a scene.") }
        func room(_ address: String) throws -> RoomID {
            guard let room = rooms[address] else {
                throw ContractError.invalid("A scene room is no longer available: \(address). Choose its current room and household.")
            }
            return room
        }
        var used = Set<RoomID>()
        let checked = try groups.map { group -> Scene.Group in
            let coordinator = try room(group.coordinator)
            for address in [group.coordinator] + group.members {
                let member = try room(address)
                guard member.household == coordinator.household else {
                    throw ContractError.invalid("Rooms in different households cannot share a scene group.")
                }
                guard used.insert(member).inserted else {
                    throw ContractError.invalid("Each room can belong to only one scene group: \(address).")
                }
            }
            let source: Scene.Source
            switch group.source.kind {
            case "keep": source = .init(kind: "keep", name: nil, uri: nil, mood: nil)
            case "favorite":
                let name = (group.source.name ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
                guard !name.isEmpty, name.utf8.count <= 4_096 else {
                    throw ContractError.invalid("Choose a playable favorite for each favorite source.")
                }
                source = .init(kind: "favorite", name: name, uri: nil, mood: nil)
            case "uri": source = .init(kind: "uri", name: nil, uri: try Self.sourceURI(group.source.uri ?? ""), mood: nil)
            case "dj":
                let mood = (group.source.mood ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
                guard mood.utf8.count <= 128 else { throw ContractError.invalid("Choose a shorter DJ mood name.") }
                source = .init(kind: "dj", name: nil, uri: nil, mood: mood.isEmpty ? nil : mood)
            default: throw ContractError.invalid("Choose a supported scene music source.")
            }
            return .init(coordinator: group.coordinator, members: group.members, source: source,
                         playing: group.playing ?? (source.kind == "keep" ? nil : true))
        }
        for (address, value) in volumes {
            _ = try room(address)
            guard (0...100).contains(value) else { throw ContractError.invalid("Room levels must be from 0 to 100.") }
        }
        for address in mutes.keys { _ = try room(address) }
        let definition = Self(groups: checked, volumes: volumes, mutes: mutes)
        _ = try definition.scene(named: "Draft").validated()
        return definition
    }

    func scene(named name: String, saved: Int64 = 0) -> Scene {
        Scene(name: name, saved: saved, groups: groups, volumes: volumes, mutes: mutes)
    }

    var roomAddresses: Set<String> {
        Set(groups.flatMap { [$0.coordinator] + $0.members } + Array(volumes.keys) + Array(mutes.keys))
    }

    /// Matches the daemon's normalization without following a pasted URL.
    static func sourceURI(_ raw: String) throws -> String {
        let value = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !value.isEmpty, value.utf8.count <= 2_048,
              !value.unicodeScalars.contains(where: { CharacterSet.whitespacesAndNewlines.contains($0) || CharacterSet.controlCharacters.contains($0) }),
              let colon = value.firstIndex(of: ":") else {
            throw ContractError.invalid("Paste one complete music URI or Spotify link, without spaces.")
        }
        let scheme = String(value[..<colon]), rest = String(value[value.index(after: colon)...])
        guard let first = scheme.utf8.first, (65...90).contains(first) || (97...122).contains(first), !rest.isEmpty,
              scheme.utf8.allSatisfy({ (65...90).contains($0) || (97...122).contains($0) || (48...57).contains($0) || [43, 45, 46].contains($0) }) else {
            throw ContractError.invalid("Paste a complete music URI including its scheme.")
        }
        func lowerASCII(_ value: String) -> String { String(decoding: SceneName.key(value), as: UTF8.self) }
        func spotify(_ kind: String, _ id: String) throws -> String {
            let kind = lowerASCII(kind)
            guard ["track", "album", "playlist"].contains(kind), id.utf8.count == 22,
                  id.utf8.allSatisfy({ (65...90).contains($0) || (97...122).contains($0) || (48...57).contains($0) }) else {
                throw ContractError.invalid("Choose a Spotify track, album or playlist with its complete 22-character ID.")
            }
            return "spotify:\(kind):\(id)"
        }
        if lowerASCII(scheme) == "spotify" {
            let parts = rest.split(separator: ":", omittingEmptySubsequences: false).map(String.init)
            if parts.count == 2 { return try spotify(parts[0], parts[1]) }
            if parts.count == 4, lowerASCII(parts[0]) == "user", lowerASCII(parts[2]) == "playlist" {
                return try spotify("playlist", parts[3])
            }
            throw ContractError.invalid("Paste a complete Spotify track, album or playlist link.")
        }
        if ["http", "https"].contains(lowerASCII(scheme)), rest.hasPrefix("//") {
            let pieces = String(rest.dropFirst(2)).split(separator: "/", maxSplits: 1, omittingEmptySubsequences: false)
            let host = lowerASCII(pieces.first.map(String.init) ?? "")
            if host == "spotify.link" || host.hasSuffix(".spotify.link") {
                throw ContractError.invalid("Open the Spotify short link, then paste the full open.spotify.com address.")
            }
            if ["open.spotify.com", "play.spotify.com"].contains(host) {
                let path = pieces.count == 2 ? String(pieces[1]) : ""
                var parts = path.prefix { $0 != "?" && $0 != "#" }.split(separator: "/").map(String.init)
                if parts.first?.hasPrefix("intl-") == true { parts.removeFirst() }
                if parts.first == "embed" { parts.removeFirst() }
                if parts.count == 2 { return try spotify(parts[0], parts[1]) }
                if parts.count == 4, parts[0] == "user", parts[2] == "playlist" { return try spotify("playlist", parts[3]) }
                throw ContractError.invalid("Paste a full Spotify track, album or playlist link.")
            }
        }
        return value
    }
}

public enum SceneMutation: Equatable, Sendable {
    case capture(name: String, replacing: Scene?)
    case save(name: String, definition: SceneDefinition, replacing: Scene?)
    case delete(Scene)

    public var name: String {
        switch self {
        case .capture(let name, _), .save(let name, _, _): name.trimmingCharacters(in: .whitespacesAndNewlines)
        case .delete(let scene): scene.name
        }
    }
    public var previous: Scene? {
        switch self { case .capture(_, let previous), .save(_, _, let previous): previous; case .delete(let scene): scene }
    }
    public var title: String {
        switch self {
        case .capture: previous == nil ? "Capture \(name)" : "Replace \(name) with this house"
        case .save: previous == nil ? "Save \(name)" : "Replace \(name) with this definition"
        case .delete: "Delete \(name)"
        }
    }
    public var isCapture: Bool { if case .capture = self { true } else { false } }
    public var isDelete: Bool { if case .delete = self { true } else { false } }
}

public struct SceneMutationReview: Identifiable, Sendable {
    public let id = UUID()
    public let command: SceneMutation
    public let observedAt = Date()
    public var name: String { command.name }
    public var previous: Scene? { command.previous }
    public var preview: Scene? { observation.preview }
    public var groups: [Zone] { observation.groups }
    let observation: SceneMutationObservation
}

public struct SceneMutationReceipt: Sendable {
    public let saved: Scene?
    public let deleted: String?
    public let title: String
    public let detail: String
}

public struct RemoteSceneMutationResult: Identifiable, Sendable {
    public let id = UUID()
    public let command: SceneMutation
    public let receipt: SceneMutationReceipt?
    public let title: String
    public let detail: String
    public let kind: RemoteResult.Kind
}

public struct SceneSourceChoices: Sendable {
    public let groupID: UUID
    public let coordinator: RoomID
    public let source: SceneSourceKind
    public let favorites: [Favorite]
    public let moods: [MoodCatalog.Mood]

    public var usableFavorites: [Favorite] {
        let counts = Dictionary(favorites.map { (SceneName.key($0.title), 1) }, uniquingKeysWith: +)
        return favorites.filter { $0.playable && counts[SceneName.key($0.title)] == 1 }
    }

    public func matches(_ group: SceneGroupDraft) -> Bool {
        group.id == groupID && group.coordinator == coordinator && group.source == source
    }
}

struct SceneMutationObservation: Sendable {
    let groups: [Zone]
    let definition: SceneDefinition?
    let preview: Scene?
    let favorites: [String: Favorite]
    let moods: [String: MoodCatalog.Mood]

    static func read(_ command: SceneMutation, client: DaemonClient) async throws -> Self {
        let name = try SceneName.validated(command.name)
        if let previous = command.previous {
            _ = try previous.validated()
            guard SceneName.key(previous.name) == SceneName.key(name) else {
                throw ContractError.invalid("The replacement name no longer refers to the reviewed scene.")
            }
        }
        if case .save(_, let definition, _) = command,
           definition.groups.isEmpty && definition.volumes.isEmpty && definition.mutes.isEmpty {
            throw ContractError.invalid("Add at least one group, room level or mute setting before saving a custom scene.")
        }
        let matches = try await client.scenes().filter { SceneName.key($0.name) == SceneName.key(name) }
        guard matches.count <= 1, matches.first == command.previous else {
            throw ContractError.invalid("A scene with this name was added, changed or removed. Refresh scenes and review the replacement again.")
        }
        if let previous = command.previous {
            guard try await client.scene(named: previous.name) == previous else {
                throw ContractError.invalid("The saved scene changed. Refresh scenes before replacing or deleting it.")
            }
        }
        if command.isDelete {
            return Self(groups: [], definition: nil, preview: nil, favorites: [:], moods: [:])
        }
        let zones = try await client.zones()
        guard !zones.isEmpty else { throw ContractError.invalid("Connect to the house before saving a scene.") }
        if command.isCapture {
            return Self(groups: zones, definition: nil, preview: nil, favorites: [:], moods: [:])
        }
        guard case .save(_, let requested, _) = command else { throw ClientError.unreadable }
        let checked = try requested.validated(zones: zones)
        var wireGroups: [Scene.Group] = [], previewGroups: [Scene.Group] = []
        var favorites: [String: Favorite] = [:], moods: [String: MoodCatalog.Mood] = [:]
        for group in checked.groups {
            var wire = group.source, preview = group.source
            if group.source.kind == "favorite", let key = group.source.name {
                let values = try await client.sceneFavorites(coordinator: group.coordinator)
                let ids = values.filter { $0.id == key }
                let matches = ids.isEmpty ? values.filter { SceneName.key($0.title) == SceneName.key(key) } : ids
                guard matches.count == 1, let favorite = matches.first, favorite.playable,
                      values.filter({ SceneName.key($0.title) == SceneName.key(favorite.title) }).count == 1 else {
                    throw ContractError.invalid("Choose a playable favorite with a unique title in \(group.coordinator)'s household.")
                }
                favorites[group.coordinator] = favorite
                preview = .init(kind: "favorite", name: favorite.title, uri: nil, mood: nil)
            } else if group.source.kind == "dj", let name = group.source.mood {
                let values = try await client.sceneMoods(coordinator: group.coordinator)
                let matches = values.filter { SceneName.key($0.name) == SceneName.key(name) }
                guard matches.count == 1, let mood = matches.first else {
                    throw ContractError.invalid("This DJ mood is no longer available. Choose a listed mood or the automatic program.")
                }
                moods[group.coordinator] = mood
                wire = .init(kind: "dj", name: nil, uri: nil, mood: mood.name); preview = wire
            }
            wireGroups.append(.init(coordinator: group.coordinator, members: group.members, source: wire, playing: group.playing))
            previewGroups.append(.init(coordinator: group.coordinator, members: group.members, source: preview, playing: group.playing))
        }
        try Task.checkCancellation()
        let relevant = zones.filter { !$0.rooms.map(\.address).allSatisfy { !checked.roomAddresses.contains($0) } }
        return Self(groups: relevant,
            definition: .init(groups: wireGroups, volumes: checked.volumes, mutes: checked.mutes),
            preview: Scene(name: name, saved: 0, groups: previewGroups, volumes: checked.volumes, mutes: checked.mutes),
            favorites: favorites, moods: moods)
    }

    func validate(_ current: Self) throws {
        guard groups.count == current.groups.count,
              groups.allSatisfy({ old in current.groups.contains { old.sameGroup(as: $0) } }),
              definition == current.definition, preview == current.preview,
              favorites == current.favorites, moods == current.moods else {
            throw ContractError.invalid("The reviewed rooms, favorite or DJ mood changed. Review the scene again; nothing was sent.")
        }
    }
}

extension SceneMutationReview {
    func receipt(_ data: Data) throws -> SceneMutationReceipt {
        if command.isDelete {
            struct Deleted: Decodable { let deleted: String; let done: String }
            let actual = try DaemonClient.decode(Deleted.self, from: data)
            guard let previous, actual.deleted.utf8.elementsEqual(previous.name.utf8),
                  !actual.done.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { throw ClientError.unreadable }
            return SceneMutationReceipt(saved: nil, deleted: actual.deleted, title: "Deleted \(actual.deleted)",
                detail: "The saved definition was removed. Current playback was not changed. Schedules using this scene name need a replacement scene or a different action.")
        }
        let actual = try DaemonClient.decode(Scene.self, from: data).validated()
        guard actual.name.utf8.elementsEqual(name.utf8) else { throw ClientError.unreadable }
        if let preview {
            let authored = actual.groups.map { group in
                Scene.Group(coordinator: group.coordinator, members: group.members,
                    source: group.source.kind == "favorite" ? .init(kind: "favorite", name: group.source.name, uri: nil, mood: group.source.mood) : group.source,
                    playing: group.playing)
            }
            guard authored == preview.groups, actual.volumes == preview.volumes, actual.mutes == preview.mutes else { throw ClientError.unreadable }
        }
        return SceneMutationReceipt(saved: actual, deleted: nil, title: "Saved \(actual.name)",
            detail: "\(actual.groups.count) group(s), \(actual.volumes.count) room level(s), \(actual.mutes.count) mute setting(s) saved. Saving does not change playback. Apply this scene separately or use it in a schedule.")
    }
}

extension DaemonClient {
    public func sceneFavorites(coordinator: String) async throws -> [Favorite] {
        let values = try await get([Favorite].self, ["favorites"], query: [.init(name: "zone", value: coordinator)])
        guard values.count <= 4_096, Set(values.map(\.id)).count == values.count,
              values.allSatisfy({ !$0.id.isEmpty && !$0.title.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty }) else {
            throw ContractError.invalid("The favorite list is incomplete. Refresh the source choices.")
        }
        return values
    }

    public func sceneMoods(coordinator: String) async throws -> [MoodCatalog.Mood] {
        let values = try await get(MoodCatalog.self, ["dj", "moods"], query: [.init(name: "zone", value: coordinator)]).moods
        guard values.count <= 256, Set(values.map { SceneName.key($0.name) }).count == values.count,
              values.allSatisfy({ !$0.name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty && $0.name.utf8.count <= 128 }) else {
            throw ContractError.invalid("The DJ mood list is incomplete. Refresh the source choices.")
        }
        return values
    }
}
