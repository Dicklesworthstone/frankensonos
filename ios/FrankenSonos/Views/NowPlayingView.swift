import SwiftUI

struct NowPlayingView: View {
    @EnvironmentObject private var store: MockZoneStore
    var body: some View {
        let zone = store.selectedZone
        ZStack {
            Color(red: 0.025, green: 0.045, blue: 0.055).ignoresSafeArea()
            ZStack {
                LinearGradient(colors: [zone.track.colors[0].opacity(0.25), .black], startPoint: .topLeading, endPoint: .bottomTrailing)
                RadialGradient(colors: [zone.track.colors[1].opacity(0.3), .clear], center: .topTrailing, startRadius: 0, endRadius: 450)
                RadialGradient(colors: [zone.track.colors[0].opacity(0.16), .clear], center: .bottomLeading, startRadius: 0, endRadius: 350)
            }
            .blur(radius: 48).overlay(Color.black.opacity(0.25)).ignoresSafeArea().accessibilityHidden(true)
            VStack(spacing: 0) {
                Capsule().fill(.white.opacity(0.35)).frame(width: 36, height: 4).padding(.top, 4)
                    .frame(maxWidth: .infinity).frame(height: 12)
                    .contentShape(Rectangle()).onTapGesture { store.isPlayerPresented = false }
                    .gesture(DragGesture().onEnded { if $0.translation.height > 30 { store.isPlayerPresented = false } })
                    .accessibilityLabel("Dismiss Now Playing").accessibilityAddTraits(.isButton)
                S1TopBar(title: "Now Playing", leading: "Close", trailing: zone.track.source, leadingAction: { store.isPlayerPresented = false })
                RoomSwitcherStrip()
                GeometryReader { geometry in
                    ScrollView(.vertical, showsIndicators: false) {
                        VStack(spacing: 0) {
                            AlbumArtworkView(track: zone.track, cornerRadius: 12)
                                .frame(width: min(geometry.size.width - 64, 276), height: min(geometry.size.width - 64, 276))
                                .shadow(color: .black.opacity(0.7), radius: 24, y: 18)
                                .padding(.top, 16).padding(.bottom, 16)
                            VStack(spacing: 5) {
                                Text(zone.track.title).s1Font(22, weight: .semibold).lineLimit(2)
                                Text("\(zone.track.artist) · \(zone.track.album)").s1Font(13).foregroundStyle(.white.opacity(0.65)).lineLimit(2)
                            }
                            .frame(maxWidth: .infinity).padding(.horizontal, 16)
                            if zone.track.duration > 0 { scrubber(zone).padding(.top, 20) }
                            else { Text("Live").s1Font(12).foregroundStyle(.white.opacity(0.65)).padding(.top, 20) }
                            transport(zone).padding(.top, 8)
                            HStack(spacing: 12) {
                                Image(systemName: "speaker.fill").font(.system(size: 13))
                                ThinSlider(value: Binding(get: { store.selectedZone.volume }, set: { store.setVolume($0, for: store.selectedZoneID) }), label: "Group volume", thumbSize: 12, onEditingChanged: { editing in
                                    if editing { store.beginZoneVolume(zone.id) } else { store.finishZoneVolume(zone.id) }
                                }).disabled(store.isOffline(zone))
                                Image(systemName: "speaker.wave.3.fill").font(.system(size: 16))
                            }
                            .padding(.horizontal, 24).frame(height: 44).padding(.top, 10)
                            bottomControls(zone).padding(.top, 12).padding(.bottom, 16)
                        }
                        .frame(maxWidth: .infinity)
                    }
                }
                S1BottomTabBar()
            }
            .foregroundStyle(.white)
        }
        .preferredColorScheme(.dark)
    }
    private func scrubber(_ zone: AudioZone) -> some View {
        VStack(spacing: 5) {
            ThinSlider(value: Binding(get: { store.elapsed / zone.track.duration }, set: { store.elapsed = $0 * zone.track.duration }), label: "Playback position", thumbSize: 8).allowsHitTesting(!store.isLive).accessibilityHidden(store.isLive)
            HStack {
                Text(time(store.elapsed))
                Spacer()
                Text("-\(time(max(0, zone.track.duration - store.elapsed)))")
            }
            .s1Font(11).monospacedDigit().foregroundStyle(.white.opacity(0.65))
        }
        .padding(.horizontal, 24)
    }
    private func time(_ value: Double) -> String { String(format: "%d:%02d", Int(value) / 60, Int(value) % 60) }
    private func transport(_ zone: AudioZone) -> some View {
        HStack(spacing: 40) {
            Button { store.advanceTrack(in: zone.id, direction: -1) } label: {
                Image(systemName: "backward.end.fill").font(.system(size: 28)).frame(width: 44, height: 56)
            }.accessibilityLabel("Previous track")
            Button { store.togglePlayback(for: zone.id) } label: {
                Image(systemName: zone.isPlaying ? "pause.fill" : "play.fill").font(.system(size: 44)).frame(width: 64, height: 56)
            }.accessibilityLabel(zone.isPlaying ? "Pause" : "Play")
            Button { store.advanceTrack(in: zone.id, direction: 1) } label: {
                Image(systemName: "forward.end.fill").font(.system(size: 28)).frame(width: 44, height: 56)
            }.accessibilityLabel("Next track")
        }
        .buttonStyle(.plain).disabled(store.isOffline(zone))
    }
    private func bottomControls(_ zone: AudioZone) -> some View {
        HStack(spacing: 12) {
            Button { store.isRoomsSheetPresented = true } label: {
                HStack(spacing: 6) {
                    Image(systemName: "hifispeaker.2").font(.system(size: 13))
                    Text(zone.displayName).s1Font(12, weight: .medium).lineLimit(1)
                    Image(systemName: "chevron.up").font(.system(size: 9))
                }
                .padding(.horizontal, 12).frame(height: 36)
                .background(.white.opacity(0.09), in: Capsule())
                .overlay { Capsule().stroke(.white.opacity(0.16), lineWidth: 0.5) }
            }.accessibilityLabel("Switch rooms and adjust volume")
            Spacer(minLength: 0)
            Button { store.isQueuePresented = true } label: { Image(systemName: "list.bullet").font(.system(size: 20)).frame(width: 44, height: 44) }.accessibilityLabel("Queue")
            Button { store.isSleepPresented = true } label: { Image(systemName: store.sleepMinutes == nil ? "moon.zzz" : "moon.zzz.fill").font(.system(size: 20)).frame(width: 44, height: 44) }.accessibilityLabel("Sleep timer")
        }
        .buttonStyle(.plain).padding(.horizontal, 16)
    }
}
