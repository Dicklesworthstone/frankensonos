import SwiftUI

struct PlaceholderView: View {
    @EnvironmentObject private var store: MockZoneStore
    let title: String
    var openPlayer: () -> Void = {}
    @State private var query = ""
    var body: some View {
        if title == "Search" {
            VStack(spacing: 8) {
                HStack {
                    Image(systemName: "magnifyingglass").foregroundStyle(.secondary)
                    TextField("Songs, artists, albums", text: $query).textFieldStyle(.plain)
                }
                .padding(12).background(Color.primary.opacity(0.06), in: RoundedRectangle(cornerRadius: 8)).padding(16)
                Text("Play on \(store.selectedZone.displayName)").s1Font(12).foregroundStyle(.secondary)
                ScrollView(.vertical, showsIndicators: false) {
                    ForEach(store.tracks.filter { query.isEmpty || "\($0.title) \($0.artist) \($0.album)".localizedCaseInsensitiveContains(query) }) { track in
                        SongRow(track: track) { store.selectTrack(track, in: store.selectedZoneID); openPlayer() }
                    }
                }
            }
        } else {
            VStack(alignment: .leading, spacing: 16) {
                Text("FrankenSonos").s1Font(22, weight: .semibold)
                Text("Mock remote · \(store.rooms.count) speakers").s1Font(14).foregroundStyle(.secondary)
                Text("Speaker controls in this sketch change local demo state.").s1Font(14).foregroundStyle(.secondary)
            }
            .padding(16).frame(maxWidth: .infinity, alignment: .leading)
        }
    }
}
