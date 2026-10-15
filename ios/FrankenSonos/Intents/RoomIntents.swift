import AppIntents

/// A room the daemon knows, so Siri can offer the names and Shortcuts can show a picker.
struct RoomEntity: AppEntity {
    static let typeDisplayRepresentation: TypeDisplayRepresentation = "Room"
    static let defaultQuery = RoomQuery()

    let id: String
    var displayRepresentation: DisplayRepresentation { DisplayRepresentation(title: "\(id)") }
}

struct RoomQuery: EntityQuery {
    func entities(for identifiers: [String]) async throws -> [RoomEntity] {
        identifiers.map(RoomEntity.init)
    }

    func suggestedEntities() async throws -> [RoomEntity] {
        guard DaemonSettings.isConfigured else { return [] }
        let rooms = (try? await DaemonClient(baseURL: DaemonSettings.url).rooms()) ?? []
        return rooms.map { RoomEntity(id: $0.name) }
    }
}

@MainActor
private func remote() -> RoomRemote {
    RoomRemote(client: DaemonSettings.isConfigured ? DaemonClient(baseURL: DaemonSettings.url) : nil)
}

/// Run a room command and say the outcome aloud. A failure is spoken, not thrown, so Siri reads our sentence.
@MainActor
private func speak(_ run: () async throws -> String) async -> some IntentResult & ProvidesDialog {
    do {
        return .result(dialog: IntentDialog(stringLiteral: try await run()))
    } catch {
        return .result(dialog: IntentDialog(stringLiteral: (error as? RoomRemoteError)?.message ?? "Something went wrong."))
    }
}

struct PauseRoomIntent: AppIntent {
    static let title: LocalizedStringResource = "Pause a room"
    @Parameter(title: "Room") var room: RoomEntity

    @MainActor func perform() async throws -> some IntentResult & ProvidesDialog {
        await speak { try await remote().pause(room: room.id) }
    }
}

struct ResumeRoomIntent: AppIntent {
    static let title: LocalizedStringResource = "Resume a room"
    @Parameter(title: "Room") var room: RoomEntity

    @MainActor func perform() async throws -> some IntentResult & ProvidesDialog {
        await speak { try await remote().resume(room: room.id) }
    }
}

struct PlayFavoriteIntent: AppIntent {
    static let title: LocalizedStringResource = "Play a Sonos favorite"
    @Parameter(title: "Favorite") var favorite: String
    @Parameter(title: "Room") var room: RoomEntity

    @MainActor func perform() async throws -> some IntentResult & ProvidesDialog {
        await speak { try await remote().playFavorite(favorite, room: room.id) }
    }
}

struct FrankenSonosShortcuts: AppShortcutsProvider {
    static var appShortcuts: [AppShortcut] {
        AppShortcut(intent: PauseRoomIntent(), phrases: ["Pause \(\.$room) in \(.applicationName)", "Pause the \(\.$room) in \(.applicationName)"],
                    shortTitle: "Pause", systemImageName: "pause.fill")
        AppShortcut(intent: ResumeRoomIntent(), phrases: ["Resume \(\.$room) in \(.applicationName)", "Play the \(\.$room) in \(.applicationName)"],
                    shortTitle: "Resume", systemImageName: "play.fill")
        AppShortcut(intent: PlayFavoriteIntent(), phrases: ["Play a favorite in \(.applicationName)"],
                    shortTitle: "Play favorite", systemImageName: "star.fill")
    }
}
