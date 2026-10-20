#if os(macOS) || os(iOS)
import SwiftUI
import FrankenSonosKit

@MainActor struct RoomPicker: View {
    @ObservedObject var bridge: RemoteBridge
    let select: (RoomID) -> Void
    let close: () -> Void

    var body: some View {
        NavigationStack {
            RoomList(bridge: bridge, select: select)
                .navigationTitle("Your rooms")
                #if os(iOS)
                .navigationBarTitleDisplayMode(.inline)
                #endif
                .toolbar {
                    ToolbarItem(placement: .confirmationAction) {
                        Button("Done", action: close)
                    }
                }
        }
        .presentationDetents([.medium, .large])
        .presentationDragIndicator(.visible)
    }
}

@MainActor struct RoomList: View {
    @ObservedObject var bridge: RemoteBridge
    let select: (RoomID) -> Void
    @State private var query = ""
    private var model: RemoteModel { bridge.model }
    private var zones: [Zone] { model.house?.snapshot?.zones ?? [] }
    private var households: [String] { Array(Set(zones.map(\.household))).sorted() }

    var body: some View {
        List {
            ForEach(households, id: \.self) { household in
                Section(household) {
                    ForEach(zones.filter { $0.household == household }) { zone in
                        ForEach(zone.members.filter(matches).sorted(), id: \.self) { room in
                            roomRow(room, in: zone)
                        }
                    }
                }
            }
        }
        .searchable(text: $query, prompt: "Find a room")
        .overlay {
            if zones.isEmpty {
                if model.house?.snapshot == nil && model.house?.problem == nil {
                    ProgressView("Finding your rooms…")
                } else {
                    EmptyMusicState(title: "Rooms unavailable", systemImage: "hifispeaker",
                                    detail: model.house?.problem ?? "The daemon has not found any rooms. Check its connection to your speakers, then refresh.")
                }
            } else if !zones.contains(where: { $0.members.contains(where: matches) }) {
                ContentUnavailableView.search(text: query)
            }
        }
        .safeAreaInset(edge: .bottom) {
            if model.house?.isFresh != true {
                Label(model.active ? "Reconnecting · showing the last room list" : "Connection suspended",
                      systemImage: "wifi.exclamationmark")
                    .font(.caption).foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity).padding(12).background(.regularMaterial)
            }
        }
    }

    private func matches(_ room: String) -> Bool {
        let search = query.trimmingCharacters(in: .whitespacesAndNewlines)
        return search.isEmpty || room.localizedCaseInsensitiveContains(search)
    }

    private func roomRow(_ room: String, in zone: Zone) -> some View {
        let id = RoomID(household: zone.household, room: room)
        let selected = model.selectedRoom == id
        let state = model.house?.snapshot?.states[zone.id]
        let unavailable = model.house?.snapshot?.failures[id] != nil
        let companions = zone.members.filter { $0 != room }
        return Button { select(id) } label: {
            HStack(spacing: 14) {
                Image(systemName: companions.isEmpty ? "hifispeaker.fill" : "hifispeaker.2.fill")
                    .font(.title3).foregroundStyle(selected ? RemoteStyle.accent : .secondary)
                    .frame(width: 32).accessibilityHidden(true)
                VStack(alignment: .leading, spacing: 5) {
                    Text(room).font(.headline).foregroundStyle(.primary)
                    Text(unavailable ? "Room unavailable" : (state?.track?.displayTitle ?? (state == nil ? "Playback unavailable" : "Ready to listen")))
                        .font(.subheadline).foregroundStyle(.secondary).lineLimit(2)
                    if !companions.isEmpty {
                        Text("With " + companions.joined(separator: ", "))
                            .font(.caption).foregroundStyle(.secondary).lineLimit(2)
                    }
                }
                Spacer(minLength: 4)
                if selected {
                    Image(systemName: "checkmark.circle.fill").foregroundStyle(RemoteStyle.accent)
                        .accessibilityLabel("Selected")
                } else if state?.isPlaying == true {
                    Image(systemName: "waveform").foregroundStyle(RemoteStyle.accent)
                        .accessibilityLabel("Playing")
                }
            }
            .frame(minHeight: 60).padding(.vertical, 6).contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .disabled(model.actionPhase == .confirming)
        .accessibilityElement(children: .combine)
        .accessibilityHint("Select this room without changing its music.")
    }
}
#endif
