#if os(macOS) || os(iOS)
import SwiftUI
import FrankenSonosKit

@MainActor struct SceneComposer: View {
    @ObservedObject var bridge: RemoteBridge
    @State private var expandedGroup: UUID?
    @State private var expandedRoom: RoomID?
    private var model: RemoteModel { bridge.model }
    private var rooms: [RoomID] {
        Array(Set(model.house?.snapshot?.zones.flatMap { Array($0.rooms) } ?? []))
            .sorted { $0.address < $1.address }
    }
    private var settingRooms: [RoomID] {
        Array(Set(rooms).union(model.sceneDraft.volumes.keys).union(model.sceneDraft.mutes.keys))
            .sorted { $0.address < $1.address }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 20) {
            VStack(alignment: .leading, spacing: 10) {
                HStack(alignment: .top) {
                    Text(model.sceneDraftReplacing == nil ? "Your custom scene" : "A new definition for this scene")
                        .font(.title2.bold()).accessibilityAddTraits(.isHeader)
                    Spacer(minLength: 8)
                    Button("Keep draft") { model.closeSceneDraft() }.buttonStyle(.bordered)
                        .accessibilityHint("Close the composer and keep these unsent settings.")
                }
                TextField("Scene name", text: Binding(get: { model.sceneDraft.name }, set: { value in
                    var draft = model.sceneDraft; draft.name = value; model.updateSceneDraft(draft)
                })).textFieldStyle(.roundedBorder).accessibilityLabel("Custom scene name")
                if let previous = model.sceneDraftReplacing {
                    Text("This starts a new definition for “\(previous.name)”. Add every setting you want to keep. The captured definition stays saved until you review and confirm its replacement.")
                        .font(.callout).foregroundStyle(.secondary)
                } else {
                    Text("Choose only the settings this scene should control. Unlisted room levels, mutes and playback settings stay unchanged.")
                        .font(.callout).foregroundStyle(.secondary)
                }
            }
            VStack(alignment: .leading, spacing: 14) {
                Label("Groups & music", systemImage: "hifispeaker.2").font(.headline)
                Text("Each group names its exact members. Applying it can separate rooms that are currently grouped together.")
                    .font(.caption).foregroundStyle(.secondary)
                ForEach(model.sceneDraft.groups) { group in
                    DisclosureGroup(isExpanded: Binding(get: { expandedGroup == group.id }, set: { expanded in
                        expandedGroup = expanded ? group.id : nil
                    })) {
                        SceneGroupEditor(bridge: bridge, id: group.id, rooms: rooms).padding(.top, 12)
                    } label: {
                        VStack(alignment: .leading, spacing: 4) {
                            Text(group.coordinator?.address ?? "Choose a leading room").font(.subheadline.weight(.semibold))
                            Text("\(group.members.count + 1) room(s) · \(sourceLabel(group.source))")
                                .font(.caption).foregroundStyle(.secondary)
                        }.padding(.vertical, 4)
                    }
                    Divider()
                }
                Button("Add a group", systemImage: "plus") {
                    var draft = model.sceneDraft
                    let group = SceneGroupDraft(); draft.groups.append(group)
                    model.updateSceneDraft(draft); expandedGroup = group.id
                }.buttonStyle(.bordered).disabled(rooms.isEmpty || model.sceneDraft.groups.count >= rooms.count)
            }
            Divider()
            VStack(alignment: .leading, spacing: 14) {
                Label("Room levels & mutes", systemImage: "speaker.wave.2").font(.headline)
                Text("These settings are optional and can be saved without changing any groups or music.")
                    .font(.caption).foregroundStyle(.secondary)
                if settingRooms.isEmpty {
                    Text("Room settings appear when the house is connected.").font(.callout).foregroundStyle(.secondary)
                }
                ForEach(settingRooms) { room in
                    DisclosureGroup(isExpanded: Binding(get: { expandedRoom == room }, set: { expanded in expandedRoom = expanded ? room : nil })) {
                        roomSettings(room).padding(.top, 12)
                    } label: {
                        VStack(alignment: .leading, spacing: 4) {
                            Text(room.address).font(.subheadline.weight(.semibold))
                            Text(roomSummary(room)).font(.caption).foregroundStyle(.secondary)
                        }.padding(.vertical, 4)
                    }
                }
            }
            Text("Saving stores this definition without changing playback. A scene with the same name will be shown in the review before replacement.")
                .font(.caption).foregroundStyle(.secondary)
            Button("Review custom scene", systemImage: "checklist") { model.prepareSceneDraft() }
                .buttonStyle(.borderedProminent).foregroundStyle(RemoteStyle.accentInk)
                .disabled(!model.canManageScenes || rooms.isEmpty)
        }.remoteCard().disabled(!model.canSchedule)
    }

    private func sourceLabel(_ source: SceneSourceKind) -> String {
        switch source { case .keep: "Keep music"; case .favorite: "Favorite"; case .uri: "Music link"; case .dj: "DJ" }
    }

    private func roomSummary(_ room: RoomID) -> String {
        let level = model.sceneDraft.volumes[room].map { "Volume \($0)" }
        let mute = model.sceneDraft.mutes[room].map { $0 ? "Muted" : "Unmuted" }
        let fields = [level, mute].compactMap { $0 }
        return fields.isEmpty ? "Leave unchanged" : fields.joined(separator: " · ")
    }

    private func roomSettings(_ room: RoomID) -> some View {
        VStack(alignment: .leading, spacing: 12) {
            if !rooms.contains(room) {
                Label("This room is unavailable. Remove its settings or reconnect it before saving.", systemImage: "exclamationmark.circle")
                    .font(.callout).foregroundStyle(.secondary)
            }
            Toggle("Set this room’s volume", isOn: Binding(get: { model.sceneDraft.volumes[room] != nil }, set: { enabled in
                var draft = model.sceneDraft
                draft.volumes[room] = enabled ? (model.house?.snapshot?.states[room]?.volume ?? 20) : nil
                model.updateSceneDraft(draft)
            }))
            if let level = model.sceneDraft.volumes[room] {
                HStack {
                    Slider(value: Binding(get: { Double(model.sceneDraft.volumes[room] ?? level) }, set: { value in
                        var draft = model.sceneDraft; draft.volumes[room] = Int(value); model.updateSceneDraft(draft)
                    }), in: 0...100, step: 1)
                    .accessibilityLabel("Saved volume for \(room.address)")
                    Text("\(level)").monospacedDigit().frame(minWidth: 32)
                }
            }
            Picker("Mute setting", selection: Binding(get: { model.sceneDraft.mutes[room].map { $0 ? "muted" : "unmuted" } ?? "keep" }, set: { value in
                var draft = model.sceneDraft; draft.mutes[room] = value == "keep" ? nil : value == "muted"
                model.updateSceneDraft(draft)
            })) {
                Text("Leave unchanged").tag("keep")
                Text("Muted").tag("muted")
                Text("Unmuted").tag("unmuted")
            }.pickerStyle(.menu).accessibilityLabel("Saved mute setting for \(room.address)")
        }
    }
}

@MainActor private struct SceneGroupEditor: View {
    @ObservedObject var bridge: RemoteBridge
    let id: UUID
    let rooms: [RoomID]
    @State private var filter = ""
    private var model: RemoteModel { bridge.model }
    private var group: SceneGroupDraft? { model.sceneDraft.groups.first { $0.id == id } }
    private var usedByOthers: Set<RoomID> {
        Set(model.sceneDraft.groups.filter { $0.id != id }.flatMap { Array($0.members) + [$0.coordinator].compactMap { $0 } })
    }
    private struct ChoiceKey: Hashable {
        let id: UUID
        let room: RoomID?
        let source: SceneSourceKind
        let active: Bool
    }

    var body: some View {
        if let group {
            VStack(alignment: .leading, spacing: 14) {
                Picker("Leading room", selection: Binding(get: { self.group?.coordinator }, set: { coordinator in
                    update { value in
                        value.coordinator = coordinator
                        value.members = value.members.filter { $0.household == coordinator?.household && $0 != coordinator }
                        value.value = ""
                    }
                })) {
                    Text("Choose room and household").tag(Optional<RoomID>.none)
                    if let chosen = group.coordinator, !rooms.contains(chosen) { Text("Unavailable: \(chosen.address)").tag(Optional(chosen)) }
                    ForEach(rooms) { room in
                        Text(room.address).tag(Optional(room)).disabled(usedByOthers.contains(room))
                    }
                }.pickerStyle(.menu)
                if let coordinator = group.coordinator {
                    VStack(alignment: .leading, spacing: 8) {
                        Text("Play together").font(.subheadline.weight(.medium))
                        let members = Array(Set(rooms.filter { $0.household == coordinator.household && $0 != coordinator }).union(group.members))
                            .sorted { $0.address < $1.address }
                        if members.isEmpty { Text("This room will play independently.").font(.caption).foregroundStyle(.secondary) }
                        ForEach(members) { room in
                            Toggle(room.address + (rooms.contains(room) ? "" : " · unavailable"), isOn: Binding(
                                get: { self.group?.members.contains(room) == true }, set: { included in
                                    update { value in if included { value.members.insert(room) } else { value.members.remove(room) } }
                                }))
                            .disabled(usedByOthers.contains(room) && !group.members.contains(room))
                        }
                    }
                }
                Picker("Music", selection: Binding(get: { self.group?.source ?? .keep }, set: { source in
                    filter = ""
                    update { value in
                        value.source = source; value.value = ""
                        if source != .keep && value.playback == .unchanged { value.playback = .playing }
                    }
                })) {
                    Text("Keep current source").tag(SceneSourceKind.keep)
                    Text("Sonos favorite").tag(SceneSourceKind.favorite)
                    Text("Music URI or Spotify link").tag(SceneSourceKind.uri)
                    Text("Your DJ").tag(SceneSourceKind.dj)
                }.pickerStyle(.menu)
                sourceFields(group)
                Picker("Playback", selection: Binding(get: { self.group?.playback ?? .unchanged }, set: { playback in update { $0.playback = playback } })) {
                    if group.source == .keep { Text("Leave unchanged").tag(ScenePlayback.unchanged) }
                    Text("Play").tag(ScenePlayback.playing)
                    Text("Pause").tag(ScenePlayback.paused)
                }.pickerStyle(.menu)
                Button("Remove this group", systemImage: "minus.circle", role: .destructive) {
                    var draft = model.sceneDraft; draft.groups.removeAll { $0.id == id }; model.updateSceneDraft(draft)
                }.buttonStyle(.bordered)
            }
            .task(id: ChoiceKey(id: id, room: group.coordinator, source: group.source, active: model.active)) { model.loadSceneChoices(for: id) }
        }
    }

    @ViewBuilder private func sourceFields(_ group: SceneGroupDraft) -> some View {
        switch group.source {
        case .keep:
            Text("Use whatever this leading room is playing when the scene is applied.")
                .font(.caption).foregroundStyle(.secondary)
        case .uri:
            TextField("Spotify track, album, playlist or music URI", text: Binding(get: { self.group?.value ?? "" }, set: { text in update { $0.value = text } }))
                .textFieldStyle(.roundedBorder).accessibilityLabel("Scene music URI or Spotify link")
                #if os(iOS)
                .textInputAutocapitalization(.never).autocorrectionDisabled().keyboardType(.URL)
                #endif
            Text("Paste a complete source address. The daemon resolves music; the app does not open the link.")
                .font(.caption).foregroundStyle(.secondary)
        case .favorite, .dj:
            if group.coordinator == nil {
                Text("Choose a leading room to see its household’s music choices.").font(.callout).foregroundStyle(.secondary)
            } else {
                if model.sceneChoicesLoading { ProgressView("Reading source choices…").font(.callout) }
                if let problem = model.sceneChoicesProblem {
                    Text(problem).font(.callout).foregroundStyle(.secondary).textSelection(.enabled)
                    Button("Try source choices again") { model.loadSceneChoices(for: id) }.buttonStyle(.bordered)
                }
                if group.source == .dj { moodChoices(group) }
                if group.source == .favorite, let choices = model.sceneSourceChoices, choices.matches(group) { favoriteChoices(group, choices: choices) }
            }
        }
    }

    private func moodChoices(_ group: SceneGroupDraft) -> some View {
        let choices = model.sceneSourceChoices.flatMap { $0.matches(group) ? $0.moods : nil } ?? []
        return VStack(alignment: .leading, spacing: 8) {
            Picker("DJ program", selection: Binding(get: { self.group?.value ?? "" }, set: { mood in update { $0.value = mood } })) {
                Text("Automatic · time-of-day program").tag("")
                if !group.value.isEmpty && !choices.contains(where: { $0.name == group.value }) {
                    Text("Not in current list: \(group.value)").tag(group.value)
                }
                ForEach(choices) { mood in Text(mood.name).tag(mood.name) }
            }.pickerStyle(.menu)
            Text("Automatic follows the daemon’s program when this scene is applied.").font(.caption).foregroundStyle(.secondary)
        }
    }

    private func favoriteChoices(_ group: SceneGroupDraft, choices: SceneSourceChoices) -> some View {
        let favorites = choices.usableFavorites
        let visible = favorites.filter { filter.isEmpty || $0.title.localizedCaseInsensitiveContains(filter) }
        return VStack(alignment: .leading, spacing: 10) {
            TextField("Find a favorite", text: $filter).textFieldStyle(.roundedBorder).accessibilityLabel("Filter scene favorites")
            if let selected = favorites.first(where: { $0.id == group.value }) {
                Label("Selected: \(selected.title)", systemImage: "checkmark.circle.fill").font(.callout)
            } else if !group.value.isEmpty {
                Text("The selected favorite is unavailable. Choose a current favorite below.").font(.callout).foregroundStyle(.secondary)
            }
            if favorites.isEmpty {
                Text("No playable favorites with unique titles were found in this household. Add or rename a favorite, then refresh these choices.")
                    .font(.callout).foregroundStyle(.secondary)
            } else if visible.isEmpty {
                Text("No favorites match this filter.").font(.callout).foregroundStyle(.secondary)
                Button("Clear filter") { filter = "" }
            }
            ForEach(Array(visible.prefix(40))) { favorite in
                Button {
                    guard model.sceneSourceChoices?.matches(group) == true,
                          self.group?.coordinator == group.coordinator, self.group?.source == group.source else { return }
                    update { $0.value = favorite.id }
                } label: {
                    HStack(alignment: .top, spacing: 10) {
                        Image(systemName: group.value == favorite.id ? "checkmark.circle.fill" : "circle")
                            .foregroundStyle(RemoteStyle.accent)
                        VStack(alignment: .leading, spacing: 3) {
                            Text(favorite.title).foregroundStyle(.primary)
                            if let detail = favorite.description, !detail.isEmpty { Text(detail).font(.caption).foregroundStyle(.secondary) }
                        }.frame(maxWidth: .infinity, alignment: .leading)
                    }.padding(.vertical, 10).contentShape(Rectangle())
                }.buttonStyle(.plain).accessibilityLabel("Choose favorite: \(favorite.title)")
            }
            if visible.count > 40 { Text("\(visible.count) favorites match. Refine the filter to find yours.").font(.caption).foregroundStyle(.secondary) }
            Button("Refresh favorites", systemImage: "arrow.clockwise") { model.loadSceneChoices(for: id) }.buttonStyle(.bordered)
        }
    }

    private func update(_ change: (inout SceneGroupDraft) -> Void) {
        var draft = model.sceneDraft
        guard let index = draft.groups.firstIndex(where: { $0.id == id }) else { return }
        change(&draft.groups[index]); model.updateSceneDraft(draft)
    }
}

@MainActor struct SceneMutationReviewSheet: View {
    let review: SceneMutationReview
    let confirm: () -> Void
    let cancel: () -> Void

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 18) {
                    Text(review.command.title).font(.title.bold()).accessibilityAddTraits(.isHeader)
                    Label("Saved house definition", systemImage: "house").font(.headline)
                    if review.command.isDelete {
                        Text("Delete this saved definition. Current playback continues. Schedules that use this name will need another scene. This store change cannot be undone from Activity.")
                    } else if review.command.isCapture {
                        Text("Capture all currently connected house groups when you confirm. The daemon reads the music, group membership, room levels and mutes at that moment. Saving does not change playback.")
                        ForEach(review.groups) { zone in
                            VStack(alignment: .leading, spacing: 4) {
                                Text(zone.id.address).font(.headline)
                                Text(zone.members.joined(separator: ", "))
                            }
                        }
                    } else if let preview = review.preview {
                        Text("Save these exact settings. Applying this scene later will use the listed group membership, sources and room settings. Unlisted settings stay unchanged.")
                        SceneDetails(scene: preview)
                    }
                    if let previous = review.previous {
                        Divider()
                        Text(review.command.isDelete ? "Definition to remove" : "Definition being replaced")
                            .font(.headline).accessibilityAddTraits(.isHeader)
                        Text(previous.name).font(.title3.weight(.semibold))
                        if !review.command.isDelete {
                            Text("Replacement removes the previous definition, including settings absent from this new one. Schedules using this name will use the replacement. This store change cannot be undone from Activity.")
                        }
                        DisclosureGroup("Show the previous settings") { SceneDetails(scene: previous).padding(.top, 12) }
                    }
                    Divider()
                    Text("The scene store and source choices are checked again before sending. Another controller can still edit between that check and the save. A lost acknowledgement will not be retried.")
                        .font(.callout).foregroundStyle(.secondary)
                }.padding(24).frame(maxWidth: .infinity, alignment: .leading)
            }
            .navigationTitle("Review scene change")
            #if os(iOS)
            .navigationBarTitleDisplayMode(.inline)
            #endif
            .safeAreaInset(edge: .bottom) { ReviewActions(confirm: confirm, cancel: cancel) }
        }.frame(maxWidth: 600)
        #if os(macOS)
        .frame(width: 600, height: 650)
        #endif
        .presentationDetents([.large]).presentationDragIndicator(.visible).interactiveDismissDisabled()
    }
}
#endif
