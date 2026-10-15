import Foundation

struct ZoneDTO: Decodable {
    let coordinator_room: String
    let members: [String]
    let transport_state: String
    let household: String
    let degraded: [String]?
}

struct RoomDTO: Decodable {
    let name: String
    let household: String
    let zone: String
    let aliases: [String]?
}

struct ZoneStateDTO: Decodable {
    let zone: ZoneDTO
    let transport_state: String
    let volume: Int?
    let track: TrackDTO?
}

struct TrackDTO: Decodable {
    let title: String?
    let creator: String?
    let album: String?
    let uri: String
    let duration_secs: Double?
    let position_secs: Double?
    let queue_position: Int?
    /// An https URL, or a path on the daemon (`/art?player=…`) that serves the speaker's art.
    let art_url: String?
}

struct FavoriteDTO: Decodable {
    let id: String
    let title: String
    let kind: String
    let description: String?
    let art_uri: String?
}

struct ZoneDeltaDTO: Decodable {
    let room: String?
    let player: String?
    let transport: String?
    let track: String?
    let volume: Int?
    let group_volume: Int?
    let queue_length: Int?
    let mute: Bool?
    let health: String?
}

struct DaemonFailure: Error, Decodable, LocalizedError {
    var status: Int = 0
    let detail: String
    let code: String?
    let hint: String?
    let suggestions: [String]?
    let retryable: Bool?
    var errorDescription: String? { [detail, hint].compactMap { $0 }.joined(separator: "\n") }
    private enum CodingKeys: String, CodingKey { case detail, code, hint, suggestions, retryable }
}
