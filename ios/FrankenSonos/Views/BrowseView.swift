import SwiftUI

struct BrowseView: View {
    @EnvironmentObject private var store: MockZoneStore
    var openPlayer: () -> Void = {}
    @ScaledMetric private var rowHeight = 56.0
    @State private var liveSource = "Favorites"
    var body: some View {
        ScrollView(.vertical, showsIndicators: false) {
            VStack(alignment: .leading, spacing: 0) {
                HStack {
                    Text("Play on \(store.selectedZone.displayName)").s1Font(11).foregroundStyle(.secondary)
                    Spacer()
                }
                .padding(.horizontal, 16).frame(height: 40)
                if store.isLive {
                    Picker("Source", selection: $liveSource) {
                        Text("Favorites").tag("Favorites")
                        Text("Spotify").tag("Spotify")
                    }
                    .pickerStyle(.segmented).padding(.horizontal, 16).padding(.bottom, 8)
                    if liveSource == "Spotify" {
                        SpotifyBrowse(openPlayer: openPlayer)
                    } else {
                        Text("Favorites").s1Font(18, weight: .semibold).padding(16)
                        ForEach(store.tracks) { track in
                            SongRow(track: track) { store.selectTrack(track, in: store.selectedZoneID); openPlayer() }
                                .disabled(store.isOffline(store.selectedZone))
                        }
                        if store.tracks.isEmpty { Text("No favorites").s1Font(13).foregroundStyle(.secondary).padding(16) }
                    }
                } else if let source = store.browseSource {
                    if source.name == "TV" {
                        VStack(spacing: 16) {
                            Image(systemName: "tv").font(.system(size: 40))
                            Text("TV audio").s1Font(22, weight: .semibold)
                            Text("Movie Room · Sonos Arc").s1Font(14).foregroundStyle(.secondary)
                            Button("Listen in Movie Room") {
                                store.selectedZoneID = store.zone(for: "Movie Room").id
                                if !store.selectedZone.isPlaying { store.togglePlayback(for: store.selectedZoneID) }
                                openPlayer()
                            }
                            .s1Font(15, weight: .medium).buttonStyle(.plain).frame(minHeight: 44)
                        }
                        .frame(maxWidth: .infinity).padding(.vertical, 32)
                    }
                    ForEach(sourceTracks(source)) { track in
                        SongRow(track: track) {
                            store.selectTrack(track, in: store.selectedZoneID)
                            openPlayer()
                        }
                    }
                } else {
                    recentlyPlayed
                    ForEach(MusicSource.all) { source in
                        Button { store.browseSource = source } label: {
                            HStack(spacing: 16) {
                                ZStack {
                                    RoundedRectangle(cornerRadius: 8, style: .continuous).fill(source.colors[0])
                                    Image(systemName: source.symbol).font(.system(size: 22, weight: .bold)).foregroundStyle(.white)
                                }
                                .frame(width: 40, height: 40)
                                Text(source.name).s1Font(15, weight: .medium)
                                Spacer()
                                Image(systemName: "chevron.right").font(.system(size: 13, weight: .medium)).foregroundStyle(.secondary)
                            }
                            .padding(.horizontal, 16).frame(minHeight: rowHeight)
                            .overlay(alignment: .bottom) { Rectangle().fill(Color.primary.opacity(0.09)).frame(height: 0.5).padding(.leading, 72) }
                            .contentShape(Rectangle())
                        }
                        .buttonStyle(.plain)
                    }
                    Text("Your music, all in one place.").s1Font(12).foregroundStyle(.secondary).padding(16).padding(.top, 8)
                }
            }
        }
    }
    /// The row of recent covers that used to sit on its own My Sonos tab.
    private var recentlyPlayed: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text("Recently Played").s1Font(18, weight: .semibold).padding(.horizontal, 16).padding(.top, 8).padding(.bottom, 16)
            ScrollView(.horizontal, showsIndicators: false) {
                HStack(alignment: .top, spacing: 12) {
                    ForEach(Array(store.tracks.prefix(6))) { track in
                        Button { store.selectTrack(track, in: store.selectedZoneID); openPlayer() } label: {
                            VStack(alignment: .leading, spacing: 7) {
                                AlbumArtworkView(track: track).frame(width: 136, height: 136)
                                Text(track.album).s1Font(13, weight: .medium).lineLimit(1)
                                Text(track.artist).s1Font(11).foregroundStyle(.secondary).lineLimit(1)
                            }
                            .frame(width: 136, alignment: .leading)
                        }
                        .buttonStyle(.plain)
                    }
                }
                .padding(.horizontal, 16).padding(.bottom, 16)
            }
        }
    }

    private func sourceTracks(_ source: MusicSource) -> [Track] {
        source.name == "Music Library" ? store.tracks : store.tracks.filter { $0.source == source.name }
    }
}

struct SongRow: View {
    let track: Track
    var action: () -> Void
    @ScaledMetric private var rowHeight = 56.0
    var body: some View {
        Button(action: action) {
            HStack(spacing: 12) {
                AlbumArtworkView(track: track).frame(width: 40, height: 40)
                VStack(alignment: .leading, spacing: 3) {
                    Text(track.title).s1Font(14, weight: .medium).lineLimit(1)
                    Text(track.artist).s1Font(12).foregroundStyle(.secondary).lineLimit(1)
                }
                Spacer(minLength: 0)
                Image(systemName: "chevron.right").font(.system(size: 11)).foregroundStyle(.secondary)
            }
            .padding(.horizontal, 16).frame(minHeight: rowHeight)
            .overlay(alignment: .bottom) { Rectangle().fill(Color.primary.opacity(0.08)).frame(height: 0.5).padding(.leading, 68) }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }
}
