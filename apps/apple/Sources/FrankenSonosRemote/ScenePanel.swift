#if os(macOS) || os(iOS)
import SwiftUI
import FrankenSonosKit

@MainActor struct ScenePanel: View {
    @ObservedObject var bridge: RemoteBridge
    private var model: RemoteModel { bridge.model }

    var body: some View {
        let zone = model.selectedZone
        VStack(alignment: .leading, spacing: 16) {
            Text("Bring saved rooms, music and levels together. Each scene affects all the rooms in its saved definition.")
                .font(.callout).foregroundStyle(.secondary)
            if model.scenes.isEmpty && !model.panelLoading && model.panelProblem == nil {
                EmptyMusicState(title: "A scene for every moment", systemImage: "sparkles",
                                detail: "Save a house scene in the web remote or CLI, then find it here.")
            }
            ForEach(model.scenes) { scene in
                VStack(alignment: .leading, spacing: 14) {
                    Text(scene.name).font(.title3.bold())
                    Label("Saved house scene", systemImage: "house")
                        .font(.caption).foregroundStyle(.secondary)
                    Button("Review scene", systemImage: "play.fill") { model.prepare(.scene(scene), in: zone) }
                        .buttonStyle(.borderedProminent).foregroundStyle(RemoteStyle.accentInk)
                        .disabled(!model.canPrepare || model.panelLoading)
                    DisclosureGroup("Rooms, music and levels") { SceneDetails(scene: scene).padding(.top, 12) }
                        .font(.callout)
                }.remoteCard()
            }
        }
    }
}

/// Both the browser and confirmation render the same immutable definition.
/// Every setting is shown, including names from another household.
@MainActor struct SceneDetails: View {
    let scene: FrankenSonosKit.Scene
    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            ForEach(Array(scene.groups.enumerated()), id: \.offset) { _, group in
                VStack(alignment: .leading, spacing: 4) {
                    Text("Coordinator: \(group.coordinator)").font(.headline)
                    Text(group.members.isEmpty ? "Plays independently" : "Members: " + group.members.joined(separator: ", "))
                    switch group.source.kind {
                    case "keep": Text("Keep the current source")
                    case "favorite": Text("Favorite: \(group.source.name ?? "")")
                    case "uri": Text("Source URI: \(group.source.uri ?? "")").textSelection(.enabled)
                    case "dj": Text("DJ mood: \(group.source.mood ?? "Time-of-day program")")
                    default: Text("Unsupported source: \(group.source.kind)")
                    }
                    Text(group.playing.map { $0 ? "Start / continue playback" : "Pause playback" } ?? "Leave play/pause state unchanged")
                        .font(.caption).foregroundStyle(.secondary)
                }
                Divider()
            }
            ForEach(scene.volumes.keys.sorted(), id: \.self) { room in
                Text("\(room): volume \(scene.volumes[room] ?? 0)")
            }
            ForEach(scene.mutes.keys.sorted(), id: \.self) { room in
                Text("\(room): \(scene.mutes[room] == true ? "muted" : "unmuted")")
            }
            if scene.groups.isEmpty && scene.volumes.isEmpty && scene.mutes.isEmpty {
                Text("This scene contains no settings; applying it should make no changes.")
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .textSelection(.enabled)
    }
}
#endif
