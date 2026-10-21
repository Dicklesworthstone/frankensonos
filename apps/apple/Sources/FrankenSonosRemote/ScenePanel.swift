#if os(macOS) || os(iOS)
import SwiftUI
import FrankenSonosKit

@MainActor struct ScenePanel: View {
    @ObservedObject var bridge: RemoteBridge
    @State private var fades: [String: UInt32] = [:]
    private var model: RemoteModel { bridge.model }

    var body: some View {
        let zone = model.selectedZone
        VStack(alignment: .leading, spacing: 16) {
            Text("Bring saved rooms, music and levels together. Each scene affects all the rooms in its saved definition.")
                .font(.callout).foregroundStyle(.secondary)
            capture
            if model.sceneComposerOpen { SceneComposer(bridge: bridge) }
            else {
                ViewThatFits(in: .horizontal) {
                    HStack(spacing: 12) { composerButtons }
                    VStack(alignment: .leading, spacing: 12) { composerButtons }
                }
            }
            if let problem = model.sceneDraftProblem {
                Label(problem, systemImage: "exclamationmark.circle").font(.callout)
                    .foregroundStyle(.red).textSelection(.enabled)
            }
            if let observed = model.scenesObservedAt {
                Text("Saved scenes · refreshed \(observed.formatted(date: .omitted, time: .shortened))")
                    .font(.headline).accessibilityAddTraits(.isHeader)
            }
            if model.scenes.isEmpty && !model.panelLoading && model.panelProblem == nil {
                EmptyMusicState(title: "A scene for every moment", systemImage: "sparkles",
                                detail: "Capture your house as it is, or create a custom scene for dinner, focused work or winding down.")
            }
            ForEach(model.scenes) { scene in
                VStack(alignment: .leading, spacing: 14) {
                    Text(scene.name).font(.title3.bold())
                    Label("Saved house scene", systemImage: "house")
                        .font(.caption).foregroundStyle(.secondary)
                    ViewThatFits(in: .horizontal) {
                        HStack(spacing: 12) { sceneActions(scene, zone: zone) }
                        VStack(alignment: .leading, spacing: 12) { sceneActions(scene, zone: zone) }
                    }
                    DisclosureGroup("Transition") {
                        VStack(alignment: .leading, spacing: 10) {
                            Toggle("Fade room levels", isOn: Binding(get: { fades[scene.id] != nil }, set: { enabled in
                                fades[scene.id] = enabled ? 5 : nil
                            }))
                            if fades[scene.id] != nil {
                                Stepper("Fade over \(fades[scene.id] ?? 5) seconds", value: Binding(
                                    get: { fades[scene.id] ?? 5 }, set: { fades[scene.id] = $0 }), in: 1...60)
                            }
                            Text("The daemon reports any fade or setting it cannot apply.")
                                .font(.caption).foregroundStyle(.secondary)
                        }.padding(.top, 10).disabled(!model.canSchedule)
                    }.font(.callout)
                    DisclosureGroup("Rooms, music and levels") { SceneDetails(scene: scene).padding(.top, 12) }
                        .font(.callout)
                }.remoteCard()
            }
        }
    }

    private var capture: some View {
        VStack(alignment: .leading, spacing: 12) {
            Label("Remember this moment", systemImage: "camera.aperture").font(.title3.bold())
            Text("Save the house’s groups, music, levels and mutes as they are when you confirm. Capturing does not change what is playing.")
                .font(.callout).foregroundStyle(.secondary)
            // Keep the actor-isolated call in the closure; passing the method
            // directly crashes Swift 6.1 IR generation on Apple platforms.
            TextField("Scene name, such as Evening", text: Binding(
                get: { model.sceneCaptureName }, set: { model.updateSceneCaptureName($0) }))
                .textFieldStyle(.roundedBorder).accessibilityLabel("Name for captured scene")
            Button("Review house capture", systemImage: "camera") { model.prepareSceneCapture() }
                .buttonStyle(.borderedProminent).foregroundStyle(RemoteStyle.accentInk)
                .disabled(!model.canManageScenes || model.house?.snapshot?.zones.isEmpty != false)
            if model.house?.snapshot?.zones.isEmpty != false {
                Text("Connect to at least one room to capture or create a scene. Saved scenes can still be inspected and deleted.")
                    .font(.caption).foregroundStyle(.secondary)
            }
        }.remoteCard()
    }

    @ViewBuilder private var composerButtons: some View {
        Button("Create custom scene", systemImage: "slider.horizontal.3") { model.beginSceneDraft() }
            .buttonStyle(.bordered).disabled(!model.canManageScenes)
        if !model.sceneDraft.name.isEmpty || !model.sceneDraft.groups.isEmpty || !model.sceneDraft.volumes.isEmpty || !model.sceneDraft.mutes.isEmpty {
            Button("Continue your draft", systemImage: "pencil") { model.openSceneDraft() }
                .buttonStyle(.bordered).disabled(!model.canSchedule)
        }
    }

    @ViewBuilder private func sceneActions(_ scene: FrankenSonosKit.Scene, zone: Zone?) -> some View {
        Button("Review scene", systemImage: "play.fill") {
            model.prepare(.scene(scene, fadeSeconds: fades[scene.id]), in: zone)
        }.buttonStyle(.borderedProminent).foregroundStyle(RemoteStyle.accentInk)
            .disabled(!model.canPrepare || model.panelLoading)
        Menu {
            Button("Replace with current house", systemImage: "camera") {
                model.updateSceneCaptureName(scene.name); model.prepareSceneCapture(replacing: scene)
            }.disabled(model.house?.snapshot?.zones.isEmpty != false)
            Button("Replace with a custom definition", systemImage: "slider.horizontal.3") { model.beginSceneDraft(replacing: scene) }
            Button("Review deletion", systemImage: "trash", role: .destructive) { model.prepareSceneDelete(scene) }
        } label: { Label("Manage", systemImage: "ellipsis").frame(minHeight: 32) }
            .buttonStyle(.bordered).disabled(!model.canManageScenes)
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
