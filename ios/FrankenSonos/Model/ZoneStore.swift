import Foundation
import Combine

@MainActor
protocol ZoneStore: ObservableObject {
    var zones: [AudioZone] { get }
    var selectedZoneID: UUID { get set }
    var rooms: [String] { get }
    var tracks: [Track] { get }
    func togglePlayback(for zoneID: UUID)
    func pauseAll()
    func setVolume(_ value: Double, for zoneID: UUID)
    func setGroupedRooms(_ roomNames: [String], basedOn zoneID: UUID)
    func selectTrack(_ track: Track, in zoneID: UUID)
    func advanceTrack(in zoneID: UUID, direction: Int)
}

enum RemoteTab: Hashable {
    case browse, rooms, search, settings
}
