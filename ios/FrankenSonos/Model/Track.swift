import Foundation
#if !LIVE_CHECKS
import SwiftUI
#endif

struct Track: Identifiable, Hashable {
    let id: Int
    let title: String
    let artist: String
    let album: String
    let source: String
    let symbol: String
    #if !LIVE_CHECKS
    let colors: [Color]
    #endif
    var mediaDuration: Double? = nil
    var artURL: URL? = nil
    var favoriteID: String? = nil
    var household: String? = nil
    var duration: Double { mediaDuration ?? Double(218 + id * 17) }

    static func live(title: String, artist: String, album: String, key: String, duration: Double = 0, artURL: URL? = nil) -> Track {
        let id = StableIdentity.number(key)
        #if LIVE_CHECKS
        return Track(id: id, title: title, artist: artist, album: album, source: "Sonos Favorites", symbol: "music.note", mediaDuration: max(0, duration), artURL: artURL)
        #else
        return Track(id: id, title: title, artist: artist, album: album, source: "Sonos Favorites", symbol: "music.note", colors: [Color(hue: Double(id % 360) / 360, saturation: 0.5, brightness: 0.45), Color(hue: Double((id + 55) % 360) / 360, saturation: 0.6, brightness: 0.8)], mediaDuration: max(0, duration), artURL: artURL)
        #endif
    }

    #if !LIVE_CHECKS
    // Fictional records. Every cover is drawn from this palette and seed.
    static let library: [Track] = [
        Track(id: 1, title: "After the Rain", artist: "North Window", album: "Quiet Architecture", source: "Spotify", symbol: "moon", colors: [Color(red: 0.12, green: 0.38, blue: 0.39), Color(red: 0.9, green: 0.49, blue: 0.29)]),
        Track(id: 2, title: "Soft Focus", artist: "Mira Sol", album: "Chromatic Hours", source: "Apple Music", symbol: "circle", colors: [.pink, .indigo]),
        Track(id: 3, title: "Golden Hour", artist: "Daybreak Ensemble", album: "Sunroom", source: "Sonos Radio", symbol: "sun.max", colors: [.orange, .yellow]),
        Track(id: 4, title: "Between Islands", artist: "Low Tide", album: "Tidal Forms", source: "Spotify", symbol: "water.waves", colors: [.teal, .mint]),
        Track(id: 5, title: "Small Hours", artist: "Elias Vale", album: "Still Life", source: "Apple Music", symbol: "moon", colors: [.brown, .orange]),
        Track(id: 6, title: "Blue Terrain", artist: "Parallel Lines", album: "Field Notes", source: "Spotify", symbol: "triangle", colors: [.blue, .cyan]),
        Track(id: 7, title: "Slow Current", artist: "Glass Harbor", album: "Drift Studies", source: "Radio by TuneIn", symbol: "wind", colors: [.gray, .teal]),
        Track(id: 8, title: "Aerial", artist: "Juniper Atlas", album: "Open Country", source: "Spotify", symbol: "leaf", colors: [.orange, .red]),
        Track(id: 9, title: "Paper Moon", artist: "Lena Fern", album: "Night Letters", source: "Apple Music", symbol: "moon", colors: [.indigo, .pink]),
        Track(id: 10, title: "Second Nature", artist: "The Amber Trio", album: "Sunday Shapes", source: "Spotify", symbol: "circle", colors: [.green, .yellow]),
        Track(id: 11, title: "First Light", artist: "Cedar & Stone", album: "Northern Air", source: "Sonos Radio", symbol: "sunrise", colors: [.cyan, .blue]),
        Track(id: 12, title: "Homeward", artist: "Orion Fields", album: "Slow Roads", source: "Line-In", symbol: "mountain.2", colors: [.purple, .orange])
    ]
    #endif
}

struct AudioZone: Identifiable {
    let id: UUID
    var roomNames: [String]
    var track: Track
    var isPlaying: Bool
    var volume: Double
    var displayName: String { roomNames.joined(separator: " + ") }
    var shortName: String { roomNames.count > 1 ? "\(roomNames[0]) + \(roomNames.count - 1)" : roomNames.first ?? "" }
}

#if !LIVE_CHECKS
struct MusicSource: Identifiable, Hashable {
    let name: String
    let symbol: String
    let colors: [Color]
    var id: String { name }
    static let all: [MusicSource] = [
        MusicSource(name: "Sonos Radio", symbol: "dot.radiowaves.left.and.right", colors: [Color(red: 0.68, green: 0.25, blue: 0.17)]),
        MusicSource(name: "Apple Music", symbol: "music.note", colors: [Color(red: 0.76, green: 0.27, blue: 0.39)]),
        MusicSource(name: "Radio by TuneIn", symbol: "radio", colors: [Color(red: 0.13, green: 0.36, blue: 0.42)]),
        MusicSource(name: "Spotify", symbol: "waveform", colors: [Color(red: 0.19, green: 0.48, blue: 0.30)]),
        MusicSource(name: "Music Library", symbol: "square.stack", colors: [Color(red: 0.38, green: 0.35, blue: 0.47)]),
        MusicSource(name: "TV", symbol: "tv", colors: [Color(red: 0.28, green: 0.31, blue: 0.34)]),
        MusicSource(name: "Line-In", symbol: "cable.connector", colors: [Color(red: 0.43, green: 0.39, blue: 0.32)])
    ]
}
#endif
