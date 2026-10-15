import SwiftUI

struct GroupRoomsSheet: View {
    @EnvironmentObject private var store: MockZoneStore
    @Environment(\.colorScheme) private var scheme
    var body: some View {
        VStack(spacing: 0) {
            SheetHandle { store.groupZone = nil }
            S1TopBar(title: "Group Rooms", leading: "Cancel", trailing: "Done", leadingAction: { store.groupZone = nil }, trailingAction: store.finishGroupEditing, trailingDisabled: store.groupEditingRooms.isEmpty)
            HStack {
                Text("Play together · Scroll rooms").s1Font(12, weight: .semibold)
                Spacer()
                Text("\(store.groupEditingRooms.count) selected").s1Font(11).foregroundStyle(S1Palette.secondary(scheme))
            }
            .padding(.horizontal, 16).frame(height: 32)
            RoomVolumeList(editing: true)
                .safeAreaInset(edge: .bottom, spacing: 0) {
                    Button { store.ungroupAll() } label: {
                        Text("Ungroup all").s1Font(14, weight: .semibold)
                            .frame(maxWidth: .infinity).frame(height: 44)
                            .background(Color.primary.opacity(0.06), in: RoundedRectangle(cornerRadius: 8))
                    }
                    .buttonStyle(.plain).padding(.horizontal, 16).padding(.vertical, 12)
                    .modifier(S1Surface())
                }
        }
        .frame(maxWidth: .infinity).frame(height: 688)
        .modifier(S1Surface())
        .clipShape(UnevenRoundedRectangle(topLeadingRadius: 16, topTrailingRadius: 16))
        .shadow(color: .black.opacity(0.3), radius: 24, y: -8)
    }
}

// The renderer sets the initial scroll position without changing the room inventory.
private struct SheetScrollEndKey: EnvironmentKey { static let defaultValue = false }
extension EnvironmentValues {
    var sheetScrollToEnd: Bool {
        get { self[SheetScrollEndKey.self] }
        set { self[SheetScrollEndKey.self] = newValue }
    }
}

struct RoomVolumeList: View {
    @EnvironmentObject private var store: MockZoneStore
    @Environment(\.sheetScrollToEnd) private var scrollToEnd
    var editing = false
    var body: some View {
        ScrollViewReader { proxy in
            ScrollView(.vertical, showsIndicators: false) {
                VStack(spacing: 4) {
                    ForEach(store.rooms, id: \.self) { room in
                        DeviceVolumeRow(room: room, editing: editing).id(room)
                    }
                }
                Color.clear.frame(height: 20).id("sheet-end")
            }
            .onAppear {
                if scrollToEnd {
                    DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) {
                        proxy.scrollTo("sheet-end", anchor: .bottom)
                    }
                }
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .clipped()
        .mask {
            VStack(spacing: 0) {
                Color.white
                LinearGradient(colors: [.white, .clear], startPoint: .top, endPoint: .bottom).frame(height: 18)
            }
        }
    }
}

struct SheetHandle: View {
    var close: () -> Void
    var body: some View {
        Capsule().fill(Color.secondary.opacity(0.45)).frame(width: 36, height: 4)
            .frame(maxWidth: .infinity).frame(height: 20)
            .contentShape(Rectangle())
            .gesture(DragGesture().onEnded { if $0.translation.height > 25 { close() } })
            .accessibilityLabel("Dismiss sheet").accessibilityAddTraits(.isButton)
            .accessibilityAction { close() }
    }
}

struct DeviceVolumeRow: View {
    @EnvironmentObject private var store: MockZoneStore
    @Environment(\.colorScheme) private var scheme
    let room: String
    var editing = false
    @ScaledMetric private var rowHeight = 60.0
    private var zone: AudioZone { store.zone(for: room) }
    private var checked: Bool { editing ? store.groupEditingRooms.contains(room) : store.selectedZone.roomNames.contains(room) }
    private var lastMember: Bool { !editing && checked && store.selectedZone.roomNames.count == 1 }
    var body: some View {
        HStack(spacing: 10) {
            Button(action: select) {
                AlbumArtworkView(track: zone.track).frame(width: 40, height: 40)
            }
            .buttonStyle(.plain).accessibilityLabel("Select \(room)")
            VStack(alignment: .leading, spacing: 1) {
                Button(action: select) {
                    HStack(spacing: 5) {
                        Text(room).s1Font(13, weight: .semibold).lineLimit(1)
                        if editing && room == store.groupZone?.roomNames.first {
                            Text("Coordinator").s1Font(8, weight: .medium).padding(.horizontal, 4).padding(.vertical, 2)
                                .background(Color.primary.opacity(0.07), in: RoundedRectangle(cornerRadius: 3))
                        }
                        Spacer(minLength: 0)
                    }
                }
                .buttonStyle(.plain)
                HStack {
                    Text(store.model(for: room)).s1Font(10).foregroundStyle(S1Palette.secondary(scheme)).lineLimit(1)
                    Spacer()
                    Text("\(Int((store.roomVolumes[room] ?? 0) * 100))%").s1Font(10).monospacedDigit().foregroundStyle(S1Palette.secondary(scheme))
                }
                ThinSlider(value: Binding(get: { store.roomVolumes[room] ?? 0.3 }, set: { store.setRoomVolume($0, room: room) }), label: "\(room) volume", thumbSize: 7, onEditingChanged: { editing in
                    if editing { store.beginVolume(room: room) } else { store.finishVolume(room: room) }
                }).frame(height: 12)
            }
            Button(action: toggleMembership) {
                Image(systemName: checked ? "checkmark.square.fill" : "square").font(.system(size: 20, weight: .light)).foregroundStyle(checked ? Color.primary : .secondary).frame(width: 32, height: 44)
            }
            .buttonStyle(.plain).disabled(lastMember).opacity(lastMember ? 0.5 : 1)
            .accessibilityLabel("Include \(room) in group").accessibilityValue(checked ? "Selected" : "Not selected")
        }
        .disabled(store.offlineRooms.contains(room)).opacity(store.offlineRooms.contains(room) ? 0.45 : 1)
        .padding(.horizontal, 16).frame(minHeight: rowHeight)
        .overlay(alignment: .bottom) { Rectangle().fill(Color.primary.opacity(0.08)).frame(height: 0.5).padding(.leading, 66) }
    }
    private func select() {
        if editing { store.toggleGroupRoom(room) }
        else { store.selectedZoneID = zone.id }
    }
    /// Outside the Group editor the checkbox adds or removes the room from the selected zone's group at once.
    private func toggleMembership() {
        if editing { store.toggleGroupRoom(room); return }
        let members = store.selectedZone.roomNames
        let next = checked ? members.filter { $0 != room } : members + [room]
        guard !next.isEmpty else { return }
        store.setGroupedRooms(next, basedOn: store.selectedZoneID)
    }
}

struct CompactRoomsSheet: View {
    @EnvironmentObject private var store: MockZoneStore
    @Environment(\.colorScheme) private var scheme
    var body: some View {
        VStack(spacing: 0) {
            #if os(macOS)
            SheetHandle { store.isRoomsSheetPresented = false }
            #endif
            S1TopBar(title: "Rooms & Volume", trailing: "Done", trailingAction: { store.isRoomsSheetPresented = false })
            HStack {
                Text("Scroll rooms · Drag up to expand").s1Font(11).foregroundStyle(S1Palette.secondary(scheme)).lineLimit(1)
                Spacer()
                Text("Group").s1Font(12, weight: .semibold)
            }
            .buttonStyle(.plain).padding(.horizontal, 16).frame(height: 32)
            RoomVolumeList()
                .safeAreaInset(edge: .bottom, spacing: 0) {
                    HStack(spacing: 8) {
                        Button { store.groupAll() } label: {
                            Label("Party mode / Group all", systemImage: "link").s1Font(12, weight: .semibold)
                                .frame(maxWidth: .infinity).frame(height: 44)
                                .background(Color.primary.opacity(0.1), in: RoundedRectangle(cornerRadius: 8))
                        }
                        Button { store.pauseAll() } label: {
                            Text("Pause all").s1Font(12, weight: .semibold).frame(width: 84, height: 44)
                                .overlay { RoundedRectangle(cornerRadius: 8).stroke(Color.primary.opacity(0.18), lineWidth: 1) }
                        }
                    }
                    .buttonStyle(.plain).padding(16).modifier(S1Surface())
                }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .modifier(S1Surface()).clipShape(UnevenRoundedRectangle(topLeadingRadius: 16, topTrailingRadius: 16))
        .shadow(color: .black.opacity(0.45), radius: 20, y: -8)
    }
}

struct PlayerUtilitySheet: View {
    @EnvironmentObject private var store: MockZoneStore
    @Environment(\.colorScheme) private var scheme
    var body: some View {
        VStack(spacing: 0) {
            SheetHandle(close: close)
            S1TopBar(title: store.isQueuePresented ? "Queue" : "Sleep Timer", trailing: "Done", trailingAction: close)
            ScrollView(.vertical, showsIndicators: false) {
                if store.isQueuePresented {
                    if store.isLive {
                        if let count = store.queueLengths[store.selectedZone.roomNames.first ?? ""] {
                            Text("\(count) tracks").s1Font(14).padding(24)
                        } else { Text("Queue count unavailable").s1Font(14).padding(24) }
                    } else {
                        ForEach(store.tracks) { track in SongRow(track: track) { store.selectTrack(track, in: store.selectedZoneID); close() } }
                    }
                } else {
                    VStack(spacing: 8) {
                        Text("Pause \(store.selectedZone.displayName) after:").s1Font(13).foregroundStyle(S1Palette.secondary(scheme)).padding(.top, 16)
                        ForEach([15, 30, 45, 60], id: \.self) { minutes in
                            Button("\(minutes) minutes") { store.setSleepTimer(minutes); close() }.s1Font(16).frame(maxWidth: .infinity, minHeight: 44)
                        }
                        Button("Turn off timer") { store.setSleepTimer(nil); close() }.frame(minHeight: 44)
                    }
                    .buttonStyle(.plain)
                }
            }
        }
        .frame(height: 420).modifier(S1Surface()).clipShape(UnevenRoundedRectangle(topLeadingRadius: 16, topTrailingRadius: 16))
    }
    private func close() { store.isQueuePresented = false; store.isSleepPresented = false }
}
