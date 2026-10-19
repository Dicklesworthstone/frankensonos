#if os(macOS)
import SwiftUI
import FrankenSonosKit

@MainActor struct ScenePanel: View {
    @ObservedObject var bridge: RemoteBridge
    private var model: RemoteModel { bridge.model }

    var body: some View {
        let zone = model.selectedZone
        VStack(alignment: .leading, spacing: 16) {
            Text("Scenes act on all the rooms in their saved definition, not just the selected group. Review the complete layout, sources and levels before applying.")
                .font(.callout).foregroundStyle(.secondary)
            if model.scenes.isEmpty && !model.panelLoading && model.panelProblem == nil {
                Text("No saved scenes. Save a house scene with the CLI or web remote, then refresh here.")
            }
            ForEach(model.scenes) { scene in
                GroupBox {
                    VStack(alignment: .leading, spacing: 12) {
                        HStack {
                            Text(scene.name).font(.headline)
                            Spacer()
                            Button("Review scene") { model.prepare(.scene(scene), in: zone) }
                                .disabled(!model.canPrepare || model.panelLoading)
                        }
                        DisclosureGroup("Saved layout and settings") { SceneDetails(scene: scene) }
                    }.frame(maxWidth: .infinity, alignment: .leading)
                }
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
