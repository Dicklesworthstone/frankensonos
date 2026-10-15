import SwiftUI
#if os(iOS)
import UIKit
#endif

struct RoomsView: View {
    @EnvironmentObject private var store: MockZoneStore
    @Environment(\.colorScheme) private var scheme
    var openPlayer: () -> Void = {}
    var body: some View {
        ScrollView(.vertical, showsIndicators: false) {
            VStack(alignment: .leading, spacing: 0) {
                HStack {
                    Text("Your rooms").s1Font(13, weight: .semibold)
                    Spacer()
                    Text("\(store.rooms.count) speakers · \(store.zones.count) rooms").s1Font(11).foregroundStyle(S1Palette.secondary(scheme))
                }
                .padding(.horizontal, 16).frame(height: 40)
                VStack(spacing: 0) {
                    ForEach(store.zones) { zone in DenseRoomRow(zone: zone) }
                }
                .padding(.horizontal, 16)
                HStack(spacing: 8) {
                    Image(systemName: "hand.draw").font(.system(size: 14))
                    Text("Drag a room onto another to group them.").s1Font(11)
                }
                .foregroundStyle(S1Palette.secondary(scheme))
                .padding(.horizontal, 16).padding(.top, 24)
                Button { store.groupAll() } label: {
                    HStack {
                        Image(systemName: "link")
                        Text("Party mode / Group all").s1Font(13, weight: .medium)
                        Spacer()
                        Image(systemName: "chevron.right").font(.system(size: 11))
                    }
                    .padding(12)
                    .background(Color.primary.opacity(0.045), in: RoundedRectangle(cornerRadius: 8))
                }
                .buttonStyle(.plain).padding(16)
            }
        }
    }
}

private struct DenseRoomRow: View {
    @EnvironmentObject private var store: MockZoneStore
    @Environment(\.colorScheme) private var scheme
    @ScaledMetric private var rowHeight = 56.0
    @State private var targeted = false
    let zone: AudioZone
    private var selected: Bool { zone.id == store.selectedZoneID }
    var body: some View {
        HStack(spacing: 10) {
            AlbumArtworkView(track: zone.track).frame(width: 40, height: 40).opacity(zone.isPlaying ? 1 : 0.52)
            VStack(alignment: .leading, spacing: 1) {
                HStack(spacing: 4) {
                    Text(zone.displayName).s1Font(14, weight: .semibold).lineLimit(1)
                    if zone.roomNames.count > 1 { Image(systemName: "link").font(.system(size: 10)).accessibilityLabel(zone.displayName) }
                    Spacer(minLength: 0)
                    if zone.isPlaying { EqualizerGlyph() }
                }
                Text(store.isOffline(zone) ? "Offline" : "\(zone.track.title) - \(zone.track.artist)").s1Font(11).foregroundStyle(S1Palette.secondary(scheme)).lineLimit(1)
                if zone.isPlaying {
                    ThinSlider(value: Binding(get: { store.zones.first(where: { $0.id == zone.id })?.volume ?? 0 }, set: { store.setVolume($0, for: zone.id) }), label: "\(zone.displayName) volume", thumbSize: 6, onEditingChanged: { editing in
                        if editing { store.beginZoneVolume(zone.id) } else { store.finishZoneVolume(zone.id) }
                    }).frame(height: 10)
                }
            }

            Button { store.togglePlayback(for: zone.id) } label: {
                Image(systemName: zone.isPlaying ? "pause.fill" : "play.fill").font(.system(size: 14)).frame(width: 36, height: 44)
            }
            .buttonStyle(.plain)
            .accessibilityLabel("\(zone.isPlaying ? "Pause" : "Play") \(zone.displayName)")
        }
        .disabled(store.isOffline(zone)).opacity(store.isOffline(zone) ? 0.45 : 1)
        .padding(.horizontal, 8)
        .frame(minHeight: rowHeight)
        .background(Color.primary.opacity(targeted ? 0.12 : (selected ? 0.025 : 0)))
        .overlay {
            RoundedRectangle(cornerRadius: 3).stroke(targeted ? Color.primary : (selected ? Color.primary.opacity(scheme == .dark ? 0.8 : 0.85) : .clear), lineWidth: targeted ? 2 : 1)
        }
        .overlay(alignment: .bottom) {
            if !selected { Rectangle().fill(Color.primary.opacity(0.08)).frame(height: 0.5).padding(.leading, 58) }
        }
        .contentShape(Rectangle())
        .onTapGesture { store.selectedZoneID = zone.id }
        .onLongPressGesture { store.beginGroupEditing(zone) }
        .draggable(zone.id.uuidString)
        .dropDestination(for: String.self) { items, _ in
            guard !store.isOffline(zone), let text = items.first, let id = UUID(uuidString: text), id != zone.id, store.zones.contains(where: { $0.id == id }) else { return false }
            store.group(id, onto: zone.id)
            #if os(iOS)
            UIImpactFeedbackGenerator(style: .light).impactOccurred()
            #endif
            return true
        } isTargeted: { targeted = $0 }
        .accessibilityAction(named: "Select room") { store.selectedZoneID = zone.id }
        .accessibilityAction(named: "Edit group") { store.beginGroupEditing(zone) }
        .allowsHitTesting(!store.isOffline(zone))
    }
}
