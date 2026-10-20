#if os(macOS) || os(iOS)
import SwiftUI
import FrankenSonosKit

@MainActor struct RemoteScreen: View {
    @ObservedObject var bridge: RemoteBridge
    let openSpotify: () -> Void
    let openActivity: () -> Void
    let disconnect: () -> Void
    @State private var destination: Destination = .listen
    @State private var musicPanel: RemoteModel.Panel = .favorites
    @State private var housePanel: RemoteModel.Panel = .scenes
    @State private var showRooms = false
    @State private var showConnection = false
    private var model: RemoteModel { bridge.model }

    private enum Destination: String, CaseIterable {
        case listen, music, dj, house
        var title: String {
            switch self { case .listen: "Listen"; case .music: "Music"; case .dj: "Your DJ"; case .house: "House" }
        }
        var symbol: String {
            switch self {
            case .listen: "play.circle.fill"
            case .music: "music.note.list"
            case .dj: "waveform"
            case .house: "house.fill"
            }
        }
    }

    private struct PlaybackIdentity: Hashable {
        let room: RoomID
        let uri: String?
        let queuePosition: UInt32?
        let duration: UInt32?
    }

    var body: some View {
        navigation
            .onChange(of: destination) { _, chosen in showPanel(for: chosen) }
            .sheet(isPresented: $showRooms) {
                RoomPicker(bridge: bridge, select: { room in
                    model.select(room); showRooms = false
                }, close: { showRooms = false })
            }
            .sheet(isPresented: $showConnection) { connectionDetails }
            .sheet(item: Binding(get: { model.review }, set: { if $0 == nil && model.review != nil { model.cancelReview() } })) { review in
                ReviewSheet(review: review, confirm: { model.confirm(review.id) }, cancel: { model.cancelReview() })
            }
            .sheet(item: Binding(get: { model.scheduleReview }, set: { if $0 == nil && model.scheduleReview != nil { model.cancelReview() } })) { review in
                ScheduleReviewSheet(review: review, confirm: { model.confirmSchedule(review.id) }, cancel: { model.cancelReview() })
            }
    }

    @ViewBuilder private var navigation: some View {
        #if os(iOS)
        TabView(selection: $destination) {
            ForEach(Destination.allCases, id: \.self) { tab in
                NavigationStack {
                    if destination == tab { page(tab) }
                }
                .tabItem { Label(tab.title, systemImage: tab.symbol) }
                .tag(tab)
            }
        }
        #else
        NavigationSplitView {
            RoomList(bridge: bridge, select: { model.select($0) })
                .navigationTitle("Your rooms")
                .navigationSplitViewColumnWidth(min: 210, ideal: 250)
        } detail: {
            NavigationStack {
                VStack(spacing: 0) {
                    Picker("Destination", selection: $destination) {
                        ForEach(Destination.allCases, id: \.self) { tab in
                            Label(tab.title, systemImage: tab.symbol).tag(tab)
                        }
                    }.pickerStyle(.segmented).padding(.horizontal, 24).padding(.top, 12)
                    page(destination)
                }
            }
        }
        #endif
    }

    private func page(_ tab: Destination) -> some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 24) {
                roomContext
                connectionStatus
                notices
                switch tab {
                case .listen:
                    if let zone = model.selectedZone {
                        nowPlaying(zone)
                        levels(zone)
                        HStack(spacing: 12) {
                            Button { musicPanel = .queue; destination = .music } label: {
                                Label("Up next", systemImage: "list.bullet")
                                    .frame(maxWidth: .infinity, minHeight: 32)
                            }
                            Button { housePanel = .sleep; destination = .house } label: {
                                Label("Sleep timer", systemImage: "moon.zzz")
                                    .frame(maxWidth: .infinity, minHeight: 32)
                            }
                        }.buttonStyle(.bordered)
                    } else { noRoom }
                case .music:
                    panelPicker("Music", selection: $musicPanel, choices: [.favorites, .library, .queue])
                    if model.panel == .queue, let zone = model.selectedZone {
                        PlaybackOrderPanel(bridge: bridge, zone: zone)
                    }
                    if model.panel == .library || model.selectedZone != nil { MusicPanels(bridge: bridge) }
                    else { noRoom }
                case .dj:
                    if model.selectedZone != nil { MusicPanels(bridge: bridge) }
                    else { noRoom }
                case .house:
                    panelPicker("House tools", selection: $housePanel, choices: [.scenes, .sleep, .schedules])
                    if model.panel == .schedules || model.selectedZone != nil { MusicPanels(bridge: bridge) }
                    else { noRoom }
                    if let zone = model.selectedZone {
                        grouping(zone)
                        SpeakerSoundPanel(bridge: bridge, zone: zone)
                    }
                    VStack(alignment: .leading, spacing: 14) {
                        Text("Your music account and history").font(.headline)
                        Button("Spotify & library", systemImage: "music.note.list", action: openSpotify)
                            .buttonStyle(.bordered).disabled(!model.canSchedule)
                        Button("Activity & listening history", systemImage: "clock.arrow.circlepath", action: openActivity)
                            .buttonStyle(.bordered).disabled(!model.canSchedule)
                    }.remoteCard()
                }
            }
            .padding(20).frame(maxWidth: 920, alignment: .leading)
            .frame(maxWidth: .infinity)
        }
        .background(RemoteStyle.background)
        .navigationTitle(tab.title)
        #if os(iOS)
        .navigationBarTitleDisplayMode(.inline)
        #endif
        .toolbar {
            ToolbarItem(placement: .primaryAction) {
                Menu {
                    Button("Refresh house", systemImage: "arrow.clockwise") { model.refresh() }
                        .disabled(!model.active)
                    Button("Spotify & library", systemImage: "music.note.list", action: openSpotify)
                        .disabled(!model.canSchedule)
                    Button("Activity & history", systemImage: "clock.arrow.circlepath", action: openActivity)
                        .disabled(!model.canSchedule)
                    Button("Connection details", systemImage: "network") { showConnection = true }
                        .disabled(model.actionPhase != .idle || model.review != nil || model.scheduleReview != nil || model.undoReview != nil)
                } label: {
                    Image(systemName: "ellipsis.circle").frame(minWidth: 44, minHeight: 44)
                }.accessibilityLabel("House options")
            }
        }
        .safeAreaInset(edge: .bottom, spacing: 0) {
            if tab != .listen, model.selectedZone != nil { miniPlayer }
        }
    }

    private var roomContext: some View {
        Button { showRooms = true } label: {
            HStack(spacing: 12) {
                Image(systemName: "hifispeaker.2.fill").foregroundStyle(RemoteStyle.accent)
                    .font(.title3).accessibilityHidden(true)
                VStack(alignment: .leading, spacing: 3) {
                    Text(model.selectedZone?.title ?? "Choose a room")
                        .font(.headline).foregroundStyle(.primary)
                        .fixedSize(horizontal: false, vertical: true)
                    if let zone = model.selectedZone {
                        Text("\(zone.household) · \(zone.members.count == 1 ? "One room" : "\(zone.members.count) rooms together")")
                            .font(.caption).foregroundStyle(.secondary)
                    }
                }
                Spacer(minLength: 8)
                Image(systemName: "chevron.down").font(.caption.weight(.semibold))
                    .foregroundStyle(.secondary).accessibilityHidden(true)
            }
            .frame(maxWidth: .infinity, minHeight: 44, alignment: .leading)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain).disabled(model.actionPhase != .idle || model.review != nil || model.scheduleReview != nil || model.undoReview != nil)
        .accessibilityHint("Choose a room or group to control.")
    }

    @ViewBuilder private var connectionStatus: some View {
        if !model.active || model.house?.isFresh != true {
            VStack(alignment: .leading, spacing: 8) {
                HStack(spacing: 10) {
                    Image(systemName: model.active ? "wifi.exclamationmark" : "pause.circle")
                    Text(model.active ? "Reconnecting to your house" : "Connection paused").font(.subheadline.bold())
                    Spacer()
                    if model.active {
                        Button("Retry") { model.refresh() }.buttonStyle(.bordered)
                    }
                }
                Text(model.house?.snapshot == nil ? "Your rooms will appear as soon as the daemon responds." : "Showing the last known state. Controls check the room again before sending a change.")
                    .font(.caption).foregroundStyle(.secondary)
                if let problem = model.house?.problem {
                    DisclosureGroup("Connection details") { Text(problem).font(.caption).textSelection(.enabled) }
                        .font(.caption)
                }
            }.remoteCard()
        }
    }

    @ViewBuilder private var notices: some View {
        if let result = model.result {
            ResultNotice(title: result.title, scope: result.scene.map { "Scene: " + $0 + " · saved house scope" } ?? result.target.address,
                         detail: result.detail, warning: result.kind != .confirmed, dismiss: { model.dismissResult() })
        }
        if let result = model.scheduleResult {
            ResultNotice(title: result.title, scope: result.command.detail, detail: result.detail,
                         warning: result.kind != .confirmed, dismiss: { model.dismissScheduleResult() })
        }
        if model.actionPhase != .idle {
            HStack(spacing: 12) {
                ProgressView().controlSize(.small)
                Text(model.actionPhase == .preparing ? "Preparing your review…" : "Sending your change…")
                    .font(.callout)
                Spacer()
                Button("Cancel") { model.cancelReview() }
                    .help("Stops unsent work. A sent change may already have taken effect; this is not Undo.")
            }.remoteCard()
        }
    }

    private var noRoom: some View {
        EmptyMusicState(title: model.selectedRoom == nil ? "Your music starts with a room" : "This room is unavailable",
                        systemImage: "hifispeaker.2",
                        detail: model.selectedRoom == nil ? "Choose a room above to browse music and start listening." : "Refresh the house or choose another room. Your selection stays attached to its original household.")
    }

    @ViewBuilder private func nowPlaying(_ zone: Zone) -> some View {
        if let state = model.selectedState {
            ViewThatFits(in: .horizontal) {
                HStack(alignment: .center, spacing: 28) {
                    PlayerArtwork(url: model.artworkURL).frame(width: 290, height: 290)
                    playerDetails(state, in: zone).frame(minWidth: 280, maxWidth: 420)
                }
                VStack(spacing: 24) {
                    PlayerArtwork(url: model.artworkURL)
                        .frame(maxWidth: 380)
                        .shadow(color: .black.opacity(0.12), radius: 20, y: 8)
                    playerDetails(state, in: zone)
                }.frame(maxWidth: 480)
            }
            .frame(maxWidth: .infinity)
            .padding(.vertical, 8)
        } else {
            EmptyMusicState(title: "Playback unavailable", systemImage: "music.note",
                            detail: model.house?.snapshot?.failures[zone.id] ?? "The room’s playback could not be read. Refresh to try again.")
        }
    }

    private func playerDetails(_ state: ZoneState, in zone: Zone) -> some View {
        VStack(alignment: .leading, spacing: 14) {
            Label(model.house?.isFresh == true ? state.transportState.capitalized : "Last known playback",
                  systemImage: state.isPlaying ? "waveform" : "music.note")
                .font(.caption.weight(.semibold)).foregroundStyle(.secondary)
            VStack(alignment: .leading, spacing: 7) {
                Text(state.track?.displayTitle ?? "Ready when you are")
                    .font(.title2.bold()).fixedSize(horizontal: false, vertical: true)
                    .accessibilityAddTraits(.isHeader)
                if let creator = state.track?.creator, !creator.isEmpty {
                    Text(creator).font(.title3).foregroundStyle(.secondary)
                }
                if let album = state.track?.album, !album.isEmpty {
                    Text(album).font(.subheadline).foregroundStyle(.secondary)
                }
                if state.track == nil {
                    Text("Choose a favorite, explore your library, or let your DJ take over.")
                        .foregroundStyle(.secondary)
                }
            }
            if state.track != nil, let timeline = model.selectedTimeline {
                PlayerTimeline(timeline: timeline, enabled: model.canPrepare) { model.prepare(.seek($0), in: zone) }
                    .id(PlaybackIdentity(room: zone.id, uri: state.track?.uri,
                                         queuePosition: state.track?.queuePosition, duration: state.track?.durationSecs))
            }
            transport(state, in: zone)
            if state.track == nil {
                Button("Find something to play") { destination = .music }
                    .buttonStyle(.bordered).frame(maxWidth: .infinity)
            }
        }
    }

    private func transport(_ state: ZoneState, in zone: Zone) -> some View {
        HStack(spacing: 28) {
            Spacer(minLength: 0)
            Button { model.prepare(.previous, in: zone) } label: {
                Image(systemName: "backward.end.fill").font(.title2).frame(width: 48, height: 56)
            }.accessibilityLabel("Previous track").accessibilityHint("Opens a review before changing playback.")
            Button { model.prepare(state.isPlaying ? .pause : .resume, in: zone) } label: {
                Image(systemName: state.isPlaying ? "pause.fill" : "play.fill")
                    .font(.system(size: 28, weight: .semibold))
                    .foregroundStyle(RemoteStyle.accentInk)
                    .frame(width: 76, height: 76)
                    .background(RemoteStyle.accent, in: Circle())
            }.accessibilityLabel(state.isPlaying ? "Pause" : "Play")
                .accessibilityHint("Opens a review for \(zone.title).")
            Button { model.prepare(.next, in: zone) } label: {
                Image(systemName: "forward.end.fill").font(.title2).frame(width: 48, height: 56)
            }.accessibilityLabel("Next track").accessibilityHint("Opens a review before changing playback.")
            Spacer(minLength: 0)
        }
        .buttonStyle(.plain).disabled(!model.canPrepare).opacity(model.canPrepare ? 1 : 0.45)
        .padding(.vertical, 8)
    }

    private func levels(_ zone: Zone) -> some View {
        VStack(alignment: .leading, spacing: 16) {
            if zone.members.count > 1 {
                if let volume = model.selectedState?.groupVolume {
                    RoomLevelControl(title: "Group volume", level: volume, enabled: model.canPrepare) {
                        model.prepare(.groupVolume($0), in: zone)
                    }.id("group-" + zone.id.address)
                }
                DisclosureGroup("Room levels · \(zone.members.count) rooms") {
                    memberLevels(zone).padding(.top, 16)
                }.font(.subheadline.weight(.medium))
            } else { memberLevels(zone) }
        }.remoteCard()
    }

    private func memberLevels(_ zone: Zone) -> some View {
        VStack(alignment: .leading, spacing: 20) {
            ForEach(zone.members, id: \.self) { room in
                let id = RoomID(household: zone.household, room: room)
                VStack(alignment: .leading, spacing: 8) {
                    if let state = model.house?.snapshot?.states[id] {
                        if let volume = state.volume {
                            RoomLevelControl(title: zone.members.count == 1 ? "Volume" : room, level: volume, enabled: model.canPrepare) {
                                model.prepare(.volume($0, room: room), in: zone)
                            }.id(id)
                        } else { Text("\(room) volume is unavailable").font(.callout).foregroundStyle(.secondary) }
                        if let mute = state.mute {
                            Button { model.prepare(.mute(!mute, room: room), in: zone) } label: {
                                Label(mute ? "Unmute \(room)" : "Mute \(room)", systemImage: mute ? "speaker.slash.fill" : "speaker.wave.2")
                                    .font(.callout).frame(minHeight: 32)
                            }.buttonStyle(.bordered).disabled(!model.canPrepare)
                        }
                    } else {
                        Label("\(room): \(model.house?.snapshot?.failures[id] ?? "not yet read")", systemImage: "exclamationmark.circle")
                            .font(.callout).foregroundStyle(.secondary)
                    }
                }
            }
        }
    }

    private func grouping(_ zone: Zone) -> some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Rooms together").font(.title3.bold())
            Text("\(zone.title) · \(zone.household)").font(.subheadline).foregroundStyle(.secondary)
            ViewThatFits(in: .horizontal) {
                HStack { groupingMenus(zone) }
                VStack(alignment: .leading) { groupingMenus(zone) }
            }
        }.remoteCard()
    }

    @ViewBuilder private func groupingMenus(_ zone: Zone) -> some View {
        Menu {
            Button("Group this household") { model.prepare(.party, in: zone) }
            ForEach(zone.members, id: \.self) { room in
                Menu(room) {
                    if zone.members.count > 1 { Button("Separate \(room)") { model.prepare(.leave(room: room), in: zone) } }
                    ForEach(destinations(zone).filter { $0.household == zone.household }) { target in
                        Button("Join \(room) to \(target.title)") { model.prepare(.join(room: room, to: target.id), in: zone) }
                    }
                }
            }
        } label: { Label("Group rooms", systemImage: "hifispeaker.2").frame(minHeight: 32) }
            .buttonStyle(.bordered).disabled(!model.canPrepare)
        Menu {
            ForEach(destinations(zone)) { target in
                let copy = target.household != zone.household
                Button("\(copy ? "Replay" : "Move") to \(target.title) · \(target.household)") {
                    model.prepare(.move(to: target.id, copy: copy), in: zone)
                }
            }
        } label: { Label("Move music", systemImage: "arrow.right.arrow.left").frame(minHeight: 32) }
            .buttonStyle(.bordered).disabled(!model.canPrepare || destinations(zone).isEmpty)
    }

    private var miniPlayer: some View {
        HStack(spacing: 12) {
            Button { destination = .listen } label: {
                HStack(spacing: 12) {
                    PlayerArtwork(url: model.artworkURL, cornerRadius: 10).frame(width: 46, height: 46)
                    VStack(alignment: .leading, spacing: 3) {
                        Text(model.selectedState?.track?.displayTitle ?? (model.selectedState == nil ? "Playback unavailable" : "Ready to listen"))
                            .font(.subheadline.weight(.semibold)).lineLimit(1).foregroundStyle(.primary)
                        Text(model.selectedZone?.title ?? "").font(.caption).lineLimit(1).foregroundStyle(.secondary)
                    }
                    Spacer(minLength: 0)
                }.contentShape(Rectangle())
            }.buttonStyle(.plain).accessibilityHint("Open Now Playing.")
            if let zone = model.selectedZone, let state = model.selectedState {
                Button { model.prepare(state.isPlaying ? .pause : .resume, in: zone) } label: {
                    Image(systemName: state.isPlaying ? "pause.fill" : "play.fill")
                        .font(.title3).frame(width: 44, height: 44)
                }.buttonStyle(.plain).disabled(!model.canPrepare)
                    .accessibilityLabel(state.isPlaying ? "Pause in \(zone.title)" : "Play in \(zone.title)")
                    .accessibilityHint("Opens a review before changing playback.")
            }
        }
        .padding(.horizontal, 18).padding(.vertical, 10)
        .background(.regularMaterial)
        .overlay(alignment: .top) { Divider() }
    }

    private var connectionDetails: some View {
        NavigationStack {
            Form {
                Section("Your daemon") {
                    Text(bridge.endpoint).font(.callout).textSelection(.enabled)
                    Label(model.house?.isFresh == true && model.active ? "Live connection" : "Connection needs attention",
                          systemImage: model.house?.isFresh == true && model.active ? "checkmark.circle" : "wifi.exclamationmark")
                    if let date = model.house?.snapshot?.observedAt {
                        LabeledContent("Last update", value: date.formatted(date: .omitted, time: .standard))
                    }
                    if let problem = model.house?.problem { Text(problem).font(.callout).textSelection(.enabled) }
                    Button("Refresh house") { model.refresh() }.disabled(!model.active)
                }
                Section {
                    Button("Change house") { showConnection = false; disconnect() }
                        .disabled(model.actionPhase == .confirming)
                } footer: {
                    Text("Music, sleep timers and schedules continue when you close or disconnect this remote.")
                }
            }
            .navigationTitle("Connection")
            #if os(iOS)
            .navigationBarTitleDisplayMode(.inline)
            #endif
            .toolbar { ToolbarItem(placement: .confirmationAction) { Button("Done") { showConnection = false } } }
        }
        .frame(maxWidth: 600)
        #if os(macOS)
        .frame(minWidth: 420, minHeight: 360)
        #endif
        .presentationDetents([.medium, .large])
    }

    private func panelPicker(_ title: String, selection: Binding<RemoteModel.Panel>, choices: [RemoteModel.Panel]) -> some View {
        Picker(title, selection: Binding(get: { selection.wrappedValue }, set: { panel in
            selection.wrappedValue = panel; model.show(panel)
        })) {
            ForEach(choices, id: \.self) { panel in
                Text(panel == .queue ? "Up next" : panel.rawValue.capitalized).tag(panel)
            }
        }.pickerStyle(.segmented)
    }

    private func showPanel(for chosen: Destination) {
        switch chosen {
        case .listen: model.show(.queue)
        case .music: model.show(musicPanel)
        case .dj: model.show(.dj)
        case .house: model.show(housePanel)
        }
    }

    private func destinations(_ zone: Zone) -> [Zone] {
        model.house?.snapshot?.zones.filter { !$0.sameGroup(as: zone) } ?? []
    }
}

@MainActor private struct ResultNotice: View {
    let title: String
    let scope: String
    let detail: String
    let warning: Bool
    let dismiss: () -> Void
    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(alignment: .top) {
                Label(title, systemImage: warning ? "exclamationmark.circle" : "checkmark.circle")
                    .font(.headline)
                Spacer()
                Button(action: dismiss) { Image(systemName: "xmark").frame(width: 32, height: 32) }
                    .buttonStyle(.plain).accessibilityLabel("Dismiss result")
            }
            Text(scope).font(.caption).foregroundStyle(.secondary)
            Text(detail).font(.callout).textSelection(.enabled)
        }.remoteCard().accessibilityElement(children: .contain)
    }
}

@MainActor private struct ReviewSheet: View {
    let review: CommandReview
    let confirm: () -> Void
    let cancel: () -> Void
    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 16) {
                    Text(review.command.title).font(.title.bold()).accessibilityAddTraits(.isHeader)
                    Text(review.command.savedScene != nil ? "Saved house scene · all saved rooms" : "\(review.zone.title) · \(review.zone.household)").font(.headline)
                    Text(review.command.explanation)
                    requestDetails
                    Divider()
                    Text("Observed: \(review.state.transportState) • \(review.state.track?.displayTitle ?? "No reported track")")
                    if let volume = review.state.volume { Text("Room level: \(volume)") }
                    if let favorite = review.favorite { Text("Favorite: \(favorite.title)") }
                    if let work = review.dj?.now { Text("DJ selection: \(work.composer) — \(work.title)") }
                    Divider()
                    Text("The current state will be checked again before sending. This review is not a reservation against another controller. A lost acknowledgement will not be retried.")
                        .font(.callout).foregroundStyle(.secondary)
                }.padding(24).frame(maxWidth: .infinity, alignment: .leading)
            }
            .navigationTitle("Review change")
            #if os(iOS)
            .navigationBarTitleDisplayMode(.inline)
            #endif
            .safeAreaInset(edge: .bottom) { ReviewActions(confirm: confirm, cancel: cancel) }
        }.frame(maxWidth: 560)
        #if os(macOS)
        .frame(width: 560, height: 600)
        #endif
        .presentationDetents([.large])
        .presentationDragIndicator(.visible)
        .interactiveDismissDisabled()
    }

    @ViewBuilder private var requestDetails: some View {
        switch review.command {
        case .playSource(let uri), .queueAdd(let uri, _):
            Text(uri).font(.callout.monospaced()).textSelection(.enabled)
        case .volume(_, let room):
            Text("Room: \(RoomID(household: review.zone.household, room: room ?? review.zone.coordinatorRoom).address)")
        case .mute(_, let room), .leave(let room): Text("Room: \(room)")
        case .join(let room, let to): Text("\(room) → \(to.address)")
        case .move(let to, _): Text("Destination: \(to.address)")
        case .seek(let seconds): Text("Requested position: \(seconds) seconds")
        case .queuePlay(let item, _), .queueRemove(let item, _):
            Text("Entry \(item.position): \(item.title)")
            Text(item.uri ?? "").font(.caption.monospaced()).textSelection(.enabled)
        case .djSteer(_, let seconds): Text("Duration: \(seconds / 60) minutes")
        case .scene(let scene): SceneDetails(scene: scene)
        case .sleep:
            if let timer = review.sleepTimer {
                Text("Existing timer: \(timer.done)")
                if let ends = timer.ends { Text("Existing pause deadline: \(ends)") }
            } else { Text("No existing timer was reported for this group.") }
        default: EmptyView()
        }
    }

}
#endif
