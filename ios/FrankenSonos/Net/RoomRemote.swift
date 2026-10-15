import Foundation

/// Pause, resume and play a favorite in a room named in words, for Siri and Shortcuts. It asks the daemon for the room
/// and favorite names, matches what was said, and only then sends a command, so a mishearing sends nothing.
@MainActor
struct RoomRemote {
    /// Nil when no daemon has been chosen in the app yet.
    let client: DaemonClient?

    func pause(room typed: String) async throws -> String {
        let room = try await resolve(room: typed)
        try await send("pause", ["zone": room])
        return "Paused \(room)."
    }

    func resume(room typed: String) async throws -> String {
        let room = try await resolve(room: typed)
        try await send("resume", ["zone": room])
        return "Resumed \(room)."
    }

    func playFavorite(_ typed: String, room typedRoom: String) async throws -> String {
        let room = try await resolve(room: typedRoom)
        let favorites = try await ask { try await $0.favorites(room: room) }
        let favorite = try Self.match(favorite: typed, in: favorites, room: room)
        try await send("play/favorite", ["zone": room, "favorite": favorite.id])
        return "Playing \(favorite.title) in \(room)."
    }

    // MARK: matching

    /// The daemon's room whose name equals `typed`, ignoring case and surrounding spaces.
    static func match(room typed: String, in rooms: [String]) throws -> String {
        let wanted = typed.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
        if let room = rooms.first(where: { $0.lowercased() == wanted }) { return room }
        throw RoomRemoteError("I don't know a room called \(typed.trimmingCharacters(in: .whitespacesAndNewlines)). Rooms: \(rooms.sorted().joined(separator: ", ")).")
    }

    /// An exact title wins; otherwise one title that starts with or contains what was said. Several are never guessed.
    static func match(favorite typed: String, in favorites: [FavoriteDTO], room: String) throws -> FavoriteDTO {
        let wanted = typed.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
        if let exact = favorites.first(where: { $0.title.lowercased() == wanted }) { return exact }
        let hits = favorites.filter { $0.title.lowercased().contains(wanted) }
        if hits.count == 1, !wanted.isEmpty { return hits[0] }
        if hits.count > 1 {
            throw RoomRemoteError("More than one favorite matches \(typed): \(hits.map(\.title).sorted().joined(separator: ", ")).")
        }
        throw RoomRemoteError("No favorite called \(typed) in \(room).")
    }

    // MARK: daemon

    private func resolve(room typed: String) async throws -> String {
        let rooms = try await ask { try await $0.rooms() }
        return try Self.match(room: typed, in: rooms.map(\.name))
    }

    private func send(_ path: String, _ body: [String: Any]) async throws {
        _ = try await ask { try await $0.command(path, body: body) }
    }

    /// Run a daemon call, turning every failure into a sentence a person can hear.
    private func ask<T>(_ call: (DaemonClient) async throws -> T) async throws -> T {
        guard let client else { throw RoomRemoteError("Open FrankenSonos and choose a daemon first.") }
        do {
            return try await call(client)
        } catch let failure as DaemonFailure {
            throw RoomRemoteError(failure.errorDescription ?? failure.detail)
        } catch {
            throw RoomRemoteError("I can't reach the daemon.")
        }
    }
}

struct RoomRemoteError: Error, LocalizedError, Equatable {
    let message: String
    init(_ message: String) { self.message = message }
    var errorDescription: String? { message }
}
