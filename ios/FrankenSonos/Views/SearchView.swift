import SwiftUI

struct SearchView: View {
    @EnvironmentObject private var store: MockZoneStore
    @Environment(\.colorScheme) private var scheme
    @State private var query = ""
    @State private var service = "All"
    @State private var recent = ["Northlight", "Piano", "Amber Fields", "Evening music"]
    var openPlayer: () -> Void = {}
    private let services = ["All", "Spotify", "Music Library", "Sonos Radio"]
    private let liveServices = ["Spotify", "Favorites"]
    /// Live mode searches all of Spotify unless the Favorites chip is picked.
    private var searchesSpotify: Bool { store.isLive && service != "Favorites" }
    private var results: [Track] {
        store.tracks.filter { track in
            (store.isLive || service == "All" || service == "Music Library" || track.source == service) &&
            (query.isEmpty || "\(track.title) \(track.artist) \(track.album)".localizedCaseInsensitiveContains(query))
        }
    }
    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 8) {
                Image(systemName: "magnifyingglass").foregroundStyle(S1Palette.secondary(scheme))
                TextField("Artists, songs, albums", text: $query).textFieldStyle(.plain).s1Font(14)
                    .accessibilityLabel("Search music")
                if !query.isEmpty {
                    Button { query = "" } label: { Image(systemName: "xmark.circle.fill").foregroundStyle(S1Palette.secondary(scheme)) }
                        .buttonStyle(.plain).accessibilityLabel("Clear search")
                }
            }
            .padding(.horizontal, 12).frame(height: 40)
            .background(S1Palette.field(scheme), in: RoundedRectangle(cornerRadius: 7))
            .padding(.horizontal, 16).padding(.top, 16)
            ScrollView(.horizontal, showsIndicators: false) {
                HStack(spacing: 8) {
                    ForEach(store.isLive ? liveServices : services, id: \.self) { name in
                        Button { service = name } label: {
                            Text(name).s1Font(12, weight: service == name ? .semibold : .regular)
                                .fixedSize().padding(.horizontal, 12).frame(height: 32)
                                .foregroundStyle(service == name ? S1Palette.chipSelectedText(scheme) : S1Palette.equalizer(scheme))
                                .background(service == name ? S1Palette.chipSelected(scheme) : S1Palette.field(scheme), in: Capsule())
                        }
                        .buttonStyle(.plain).accessibilityAddTraits(service == name ? .isSelected : [])
                    }
                }
                .padding(.horizontal, 16)
            }
            .padding(.vertical, 12)
            ScrollView(.vertical, showsIndicators: false) {
                VStack(alignment: .leading, spacing: 0) {
                    if searchesSpotify {
                        SpotifySearchResultsView(query: query, openPlayer: openPlayer)
                    } else if query.isEmpty && !store.isLive {
                        HStack {
                            Text("Recent Searches").s1Font(17, weight: .semibold)
                            Spacer()
                            Button("Clear") { recent = [] }.s1Font(12).buttonStyle(.plain)
                        }
                        .padding(.horizontal, 16).frame(height: 44)
                        ForEach(store.isLive ? [] : recent, id: \.self) { text in
                            Button { query = text } label: {
                                HStack(spacing: 12) {
                                    Image(systemName: "clock").font(.system(size: 18, weight: .light)).foregroundStyle(S1Palette.secondary(scheme))
                                    Text(text).s1Font(14)
                                    Spacer()
                                    Image(systemName: "arrow.up.left").font(.system(size: 12)).foregroundStyle(S1Palette.secondary(scheme))
                                }
                                .padding(.horizontal, 16).frame(height: 52)
                                .overlay(alignment: .bottom) { Rectangle().fill(Color.primary.opacity(0.1)).frame(height: 0.5).padding(.leading, 46) }
                                .contentShape(Rectangle())
                            }
                            .buttonStyle(.plain)
                        }
                        Text(store.isLive ? "Search your favorites" : "Search your music services").s1Font(17, weight: .semibold)
                            .padding(.horizontal, 16).padding(.top, 26).padding(.bottom, 8)
                    } else {
                        Text("Songs").s1Font(17, weight: .semibold).padding(16)
                    }
                    if !searchesSpotify {
                        ForEach(results) { track in
                            SongRow(track: track) {
                                if !query.isEmpty && !recent.contains(query) { recent.insert(query, at: 0) }
                                store.selectTrack(track, in: store.selectedZoneID)
                                openPlayer()
                            }
                        }
                        if results.isEmpty { Text("No songs found").s1Font(13).foregroundStyle(S1Palette.secondary(scheme)).padding(16) }
                    }
                }
                .padding(.bottom, 16)
            }
        }
    }
}
