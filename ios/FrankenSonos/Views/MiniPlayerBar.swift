import SwiftUI

struct MiniPlayerBar: View {
    @EnvironmentObject private var store: MockZoneStore
    var openPlayer: () -> Void
    var body: some View {
        let zone = store.selectedZone
        HStack(spacing: 10) {
            Button(action: openPlayer) {
                HStack(spacing: 10) {
                    AlbumArtworkView(track: zone.track).frame(width: 40, height: 40)
                    VStack(alignment: .leading, spacing: 4) {
                        Text(zone.displayName).s1Font(13, weight: .semibold).lineLimit(1)
                        Text("\(zone.track.title) - \(zone.track.artist)").s1Font(11).foregroundStyle(.white.opacity(0.65)).lineLimit(1)
                    }
                    Spacer(minLength: 0)
                }
                .contentShape(Rectangle())
            }
            .accessibilityLabel("Now playing in \(zone.displayName)")
            Button { store.togglePlayback(for: zone.id) } label: {
                Image(systemName: zone.isPlaying ? "pause.fill" : "play.fill").font(.system(size: 18)).frame(width: 44, height: 44)
            }
            .accessibilityLabel(zone.isPlaying ? "Pause" : "Play").disabled(store.isOffline(zone))
        }
        .buttonStyle(.plain).padding(.horizontal, 16).frame(minHeight: 64)
        .foregroundStyle(.white).background(Color(white: 0.14))
        .overlay(alignment: .top) { Rectangle().fill(.white.opacity(0.08)).frame(height: 0.5) }
    }
}
