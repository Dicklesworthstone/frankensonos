#if os(macOS) || os(iOS)
import SwiftUI
import FrankenSonosKit

@MainActor struct MusicPanels: View {
    @ObservedObject var bridge: RemoteBridge
    @State private var search = ""
    @State private var source = ""
    @State private var mood = ""
    @State private var minutes = 60
    @State private var browsingLibrary = true
    @State private var selectingQueueBlock = false
    @State private var queueSelectionRefreshID = UUID()
    private var model: RemoteModel { bridge.model }
    private var ready: Bool { model.canPrepare && !model.panelLoading }
    private var catalogVisible: Bool { model.panel == .library && browsingLibrary }
    private var panelLoading: Bool { catalogVisible ? bridge.catalog.loading : model.panelLoading }
    private var panelTitle: String {
        switch model.panel {
        case .queue: "Up next"
        case .favorites: "Your favorites"
        case .dj: "A soundtrack for you"
        case .library: "Explore your music"
        case .scenes: "Scenes"
        case .sleep: "Sleep timer"
        case .schedules: "Schedules"
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            HStack {
                Text(panelTitle).font(.title2.bold()).accessibilityAddTraits(.isHeader)
                Spacer()
                if panelLoading { ProgressView().controlSize(.small) }
                Button {
                    if catalogVisible { bridge.catalog.refresh() }
                    else {
                        if model.panel == .queue { bridge.queueSearch.invalidate(); queueSelectionRefreshID = UUID() }
                        model.refreshPanel()
                    }
                } label: {
                    Image(systemName: "arrow.clockwise").frame(width: 44, height: 44)
                }
                .buttonStyle(.plain)
                .disabled(catalogVisible ? !bridge.catalog.canRefresh : !model.active || panelLoading)
                .accessibilityLabel("Refresh \(panelTitle.lowercased())")
            }
            if let problem = model.panelProblem, !catalogVisible {
                Label(problem, systemImage: "exclamationmark.circle")
                    .font(.callout).foregroundStyle(.secondary).textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true).remoteCard()
            }
            switch model.panel {
            case .queue: queuePanel
            case .favorites: favoritesPanel
            case .dj:
                DJRehearsalPanel(bridge: bridge)
                djPanel
                if let zone = model.selectedZone, let status = model.dj {
                    DJSteeringPanel(bridge: bridge, zone: zone, status: status)
                }
            case .library: libraryPanel
            case .scenes: ScenePanel(bridge: bridge)
            case .sleep: SleepPanel(bridge: bridge)
            case .schedules: SchedulesPanel(bridge: bridge)
            }
        }
    }

    @ViewBuilder private var queuePanel: some View {
        QueueSearchPanel(bridge: bridge).disabled(selectingQueueBlock)
        QueueBlocksPanel(bridge: bridge, selecting: $selectingQueueBlock, refreshID: queueSelectionRefreshID)
        if let page = model.queue {
            if !selectingQueueBlock {
                if page.items.isEmpty {
                    EmptyMusicState(title: page.total == 0 ? "Make room for a little music" : "This page is empty",
                                    systemImage: "text.line.first.and.arrowtriangle.forward",
                                    detail: page.total == 0 ? "Find something in Music, or add a link below. Your next tracks will appear here." : "The queue may have changed. Refresh or return to the previous page.")
                } else {
                    Text("\(page.total) \(page.total == 1 ? "track" : "tracks") in this group’s queue")
                        .font(.subheadline).foregroundStyle(.secondary)
                    LazyVStack(spacing: 10) {
                        ForEach(page.items) { item in queueRow(item, page: page) }
                    }
                }
            }
            if page.revision?.isEmpty != false {
                Label("Queue editing is unavailable until a fresh queue revision can be read.", systemImage: "info.circle")
                    .font(.caption).foregroundStyle(.secondary)
            }
            if page.total > 25 || model.queueOffset > 0 {
                HStack {
                    Button { model.queuePage(offset: model.queueOffset >= 25 ? model.queueOffset - 25 : 0) } label: {
                        Label("Previous", systemImage: "chevron.left").frame(minHeight: 32)
                    }.disabled(model.queueOffset == 0 || model.panelLoading)
                    Spacer(minLength: 8)
                    Button {
                        if let next = UInt32(exactly: UInt64(model.queueOffset) + 25) { model.queuePage(offset: next) }
                    } label: {
                        Label("Next", systemImage: "chevron.right").frame(minHeight: 32)
                    }.disabled(UInt64(model.queueOffset) + 25 >= UInt64(page.total) || model.panelLoading)
                }.buttonStyle(.bordered)
            }
        } else if model.panelLoading {
            ProgressView("Reading your queue…").frame(maxWidth: .infinity).padding(24)
        }
        sourceEntry.disabled(selectingQueueBlock)
    }

    private func queueRow(_ item: QueueItem, page: QueuePage) -> some View {
        let current = page.playing == item.position
        let editable = ready && item.playable && page.revision?.isEmpty == false
        return HStack(spacing: 12) {
            Group {
                if current {
                    Image(systemName: "waveform").foregroundStyle(RemoteStyle.accent)
                        .accessibilityLabel("Now playing")
                } else { Text("\(item.position)").font(.caption.monospacedDigit()).foregroundStyle(.secondary) }
            }.frame(width: 28)
            Button {
                if let revision = page.revision { model.prepare(.queuePlay(item, revision: revision)) }
            } label: {
                VStack(alignment: .leading, spacing: 5) {
                    Text(item.title).font(.headline).foregroundStyle(.primary).multilineTextAlignment(.leading)
                    Text([item.creator, item.album].compactMap { $0 }.filter { !$0.isEmpty }.joined(separator: " · "))
                        .font(.subheadline).foregroundStyle(.secondary).multilineTextAlignment(.leading)
                    if current { Text("Now playing").font(.caption.weight(.semibold)).foregroundStyle(RemoteStyle.accent) }
                }.frame(maxWidth: .infinity, minHeight: 44, alignment: .leading).contentShape(Rectangle())
            }.buttonStyle(.plain).disabled(!editable)
                .accessibilityHint("Review playing this queue entry.")
            Menu {
                Button("Play now", systemImage: "play") {
                    if let revision = page.revision { model.prepare(.queuePlay(item, revision: revision)) }
                }
                queueMoveMenu(item, page: page)
                Button("Remove from queue", systemImage: "minus.circle", role: .destructive) {
                    if let revision = page.revision { model.prepare(.queueRemove(item, revision: revision)) }
                }
            } label: {
                Image(systemName: "ellipsis").frame(width: 44, height: 44)
            }.disabled(!editable).accessibilityLabel("Options for \(item.title)")
        }
        .padding(14).background(current ? RemoteStyle.accent.opacity(0.10) : RemoteStyle.surface,
                                in: RoundedRectangle(cornerRadius: 18))
    }

    private func queueMoveMenu(_ item: QueueItem, page: QueuePage) -> some View {
        let zone = model.selectedZone
        return Menu("Move in queue", systemImage: "arrow.up.arrow.down") {
            ForEach(QueueMoveDestination.allCases, id: \.self) { destination in
                let move = try? QueueMove(item, in: page, to: destination)
                Button(destination.title, systemImage: queueMoveSymbol(destination)) {
                    // A menu belongs to this exact page and selected group.
                    // Fresh preflight also checks the source and destination.
                    guard ready, model.panel == .queue, model.queue == page,
                          let move, let zone else { return }
                    model.prepare(.queueMove(move), in: zone)
                }.disabled(move == nil)
            }
        }
    }

    private func queueMoveSymbol(_ destination: QueueMoveDestination) -> String {
        switch destination {
        case .earlier: "arrow.up"
        case .later: "arrow.down"
        case .afterCurrent: "text.line.first.and.arrowtriangle.forward"
        case .start: "arrow.up.to.line"
        case .end: "arrow.down.to.line"
        }
    }

    @ViewBuilder private var favoritesPanel: some View {
        Text("The music and stations saved to this household.")
            .font(.subheadline).foregroundStyle(.secondary)
        if model.favorites.isEmpty && !model.panelLoading && model.panelProblem == nil {
            EmptyMusicState(title: "Keep your favorites close", systemImage: "heart",
                            detail: "Favorites saved in Sonos appear here for this household. Refresh after adding one.")
        }
        Text("Queue tracks, albums and playlists without replacing current music. Stations play directly.")
            .font(.caption).foregroundStyle(.secondary)
        if let zone = model.selectedZone {
            LazyVStack(spacing: 12) {
                ForEach(model.favorites) { favorite in favoriteRow(favorite, in: zone) }
            }
        }
    }

    private func favoriteRow(_ favorite: Favorite, in zone: Zone) -> some View {
        HStack(spacing: 12) {
            Button {
                guard ready, model.panel == .favorites, model.favorites.contains(favorite) else { return }
                model.prepare(.playFavorite(favorite.id), in: zone)
            } label: { favoriteLabel(favorite) }
                .buttonStyle(.plain).disabled(!ready || !favorite.playable)
                .accessibilityHint("Review playing this favorite in the selected group.")
            if FavoriteQueue.canQueue(favorite) { favoriteQueueMenu(favorite, in: zone) }
        }.remoteCard()
    }

    private func favoriteLabel(_ favorite: Favorite) -> some View {
        HStack(spacing: 16) {
            Image(systemName: favorite.kind == "stream" ? "radio.fill" : "music.note")
                .font(.title2).foregroundStyle(RemoteStyle.accent)
                .frame(width: 52, height: 52)
                .background(RemoteStyle.accent.opacity(0.1), in: RoundedRectangle(cornerRadius: 14))
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 6) {
                Text(favorite.title).font(.headline).foregroundStyle(.primary)
                if let description = favorite.description, !description.isEmpty {
                    Text(description).font(.subheadline).foregroundStyle(.secondary)
                }
                if !favorite.playable { Text("Playback unavailable").font(.caption).foregroundStyle(.secondary) }
            }.frame(maxWidth: .infinity, alignment: .leading).multilineTextAlignment(.leading)
            Image(systemName: "play.circle.fill").font(.title2).foregroundStyle(RemoteStyle.accent)
                .accessibilityHidden(true)
        }.frame(minHeight: 44).contentShape(Rectangle())
    }

    private func favoriteQueueMenu(_ favorite: Favorite, in zone: Zone) -> some View {
        let next = try? FavoriteQueue(favorite, in: zone, next: true)
        let append = try? FavoriteQueue(favorite, in: zone, next: false)
        return Menu {
            Button("Queue next", systemImage: "text.line.first.and.arrowtriangle.forward") {
                if let next { prepareFavoriteQueue(next) }
            }.disabled(next == nil)
            Button("Add to end of queue", systemImage: "text.append") {
                if let append { prepareFavoriteQueue(append) }
            }.disabled(append == nil)
        } label: {
            Image(systemName: "text.badge.plus").frame(width: 44, height: 44)
        }.buttonStyle(.bordered).disabled(!ready)
            .accessibilityLabel("Queue options for \(favorite.title)")
            .accessibilityHint("Review adding this favorite without starting playback.")
    }

    private func prepareFavoriteQueue(_ selection: FavoriteQueue) {
        // A menu keeps the group and favorite that were actually displayed;
        // the shared writer also reads both again before review and dispatch.
        guard ready, model.panel == .favorites, model.favorites.contains(selection.favorite) else { return }
        model.prepare(.queueFavorite(selection), in: selection.zone)
    }

    @ViewBuilder private var djPanel: some View {
        if let status = model.dj {
            VStack(alignment: .leading, spacing: 16) {
                Label(status.running ? "Your DJ is on air" : "Ready for your next session", systemImage: "waveform")
                    .font(.subheadline.weight(.semibold)).foregroundStyle(RemoteStyle.accent)
                if let work = status.now {
                    Text(work.title).font(.title2.bold()).fixedSize(horizontal: false, vertical: true)
                    Text(work.composer).font(.title3).foregroundStyle(.secondary)
                    if let movement = work.movement, work.movements > 1 {
                        Text("Movement \(movement) of \(work.movements)").font(.subheadline).foregroundStyle(.secondary)
                    }
                    Text(work.reason.summary).font(.callout).foregroundStyle(.secondary)
                } else {
                    Text("A mix shaped by what you save and play.").font(.title3)
                    Text("Choose a mood or let the time-of-day program set the tone.").foregroundStyle(.secondary)
                }
                if let problem = status.problem {
                    Label(problem, systemImage: "exclamationmark.circle").font(.callout).textSelection(.enabled)
                }
                if status.running {
                    ViewThatFits(in: .horizontal) {
                        HStack { djFeedback }
                        VStack(alignment: .leading) { djFeedback }
                    }
                }
            }.remoteCard()
            VStack(alignment: .leading, spacing: 14) {
                Text(status.running ? "Shape the session" : "Set the mood").font(.headline)
                Picker("Mood", selection: $mood) {
                    Text("Automatic · time of day").tag("")
                    if !mood.isEmpty && !model.moods.contains(where: { $0.name == mood }) {
                        Text("Unavailable: \(mood)").tag(mood)
                    }
                    ForEach(model.moods) { Text($0.name.replacingOccurrences(of: "-", with: " ").capitalized).tag($0.name) }
                }.pickerStyle(.menu)
                if status.running {
                    Picker("Keep this mood for", selection: $minutes) {
                        ForEach([15, 30, 60, 120, 240], id: \.self) { Text($0 >= 60 ? "\($0 / 60) \($0 == 60 ? "hour" : "hours")" : "\($0) minutes").tag($0) }
                    }.pickerStyle(.menu)
                    Button(mood.isEmpty ? "Use automatic program" : "Apply mood") {
                        if mood.isEmpty { model.prepare(.djClear) }
                        else { model.prepare(.djSteer(mood: mood, seconds: UInt32(minutes * 60))) }
                    }
                    .buttonStyle(.borderedProminent).foregroundStyle(RemoteStyle.accentInk)
                    .disabled(!ready || (!mood.isEmpty && !model.moods.contains { $0.name == mood }))
                    Text("Current mood: \(status.steering.mood ?? status.steering.source). New choices shape future picks; queued works stay whole.")
                        .font(.caption).foregroundStyle(.secondary)
                    Button("Stop DJ", systemImage: "stop.circle") { model.prepare(.djStop) }
                        .buttonStyle(.bordered).disabled(!ready)
                } else {
                    Button { model.prepare(.djStart(mood: mood.isEmpty ? nil : mood)) } label: {
                        Label("Start your DJ", systemImage: "play.fill").frame(maxWidth: .infinity, minHeight: 32)
                    }
                    .buttonStyle(.borderedProminent).foregroundStyle(RemoteStyle.accentInk)
                    .disabled(!ready || (!mood.isEmpty && !model.moods.contains { $0.name == mood }))
                }
            }.remoteCard()
            if !status.next.isEmpty {
                Text("Coming next").font(.headline)
                ForEach(Array(status.next.enumerated()), id: \.offset) { index, work in
                    HStack(alignment: .top, spacing: 14) {
                        Text("\(index + 1)").font(.caption.monospacedDigit()).foregroundStyle(.secondary).frame(width: 24)
                        VStack(alignment: .leading, spacing: 4) {
                            Text(work.title).font(.headline)
                            Text(work.composer).font(.subheadline).foregroundStyle(.secondary)
                            if work.movements > 1 { Text("\(work.movements) movements · played together").font(.caption).foregroundStyle(.secondary) }
                        }
                    }.frame(maxWidth: .infinity, alignment: .leading)
                    Divider()
                }
            }
        } else if model.panelLoading {
            ProgressView("Reading your DJ session…").frame(maxWidth: .infinity).padding(24)
        } else if model.panelProblem == nil {
            EmptyMusicState(title: "Meet your personal DJ", systemImage: "waveform",
                            detail: "Refresh to read the DJ session for this group.")
        }
    }

    @ViewBuilder private var djFeedback: some View {
        Button { model.prepare(.djFeedback(like: true)) } label: { Label("More like this", systemImage: "hand.thumbsup") }
            .buttonStyle(.bordered).disabled(!ready)
        Menu {
            Button("Less like this", systemImage: "hand.thumbsdown") { model.prepare(.djFeedback(like: false)) }
            Button("Skip this work", systemImage: "forward.end") { model.prepare(.djSkip) }
        } label: { Label("More", systemImage: "ellipsis") }
            .buttonStyle(.bordered).disabled(!ready)
    }

    private var libraryPanel: some View {
        VStack(alignment: .leading, spacing: 16) {
            ViewThatFits(in: .horizontal) {
                HStack(alignment: .firstTextBaseline, spacing: 12) {
                    libraryModeDescription
                    Spacer(minLength: 0)
                    libraryModeButton
                }
                VStack(alignment: .leading, spacing: 10) {
                    libraryModeDescription
                    libraryModeButton
                }
            }
            if browsingLibrary { LibraryBrowserPanel(bridge: bridge) }
            else if model.selectedZone != nil { librarySearchPanel }
            else {
                EmptyMusicState(title: "Choose a room to search everything", systemImage: "hifispeaker",
                                detail: "Search includes favorites from the selected household. You can browse saved albums, tracks and playlists without choosing a room.")
            }
            sourceEntry
        }
    }

    private var libraryModeDescription: some View {
        Text(browsingLibrary ? "Albums, tracks and playlists" : "Your library and this household’s favorites")
            .font(.subheadline).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
    }

    private var libraryModeButton: some View {
        Button(browsingLibrary ? "Search all" : "Browse library",
               systemImage: browsingLibrary ? "magnifyingglass" : "square.grid.2x2") {
            browsingLibrary.toggle()
        }.buttonStyle(.bordered)
    }

    private var librarySearchPanel: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack(spacing: 10) {
                Image(systemName: "magnifyingglass").foregroundStyle(.secondary).accessibilityHidden(true)
                TextField("Artists, albums and tracks", text: $search)
                    .textFieldStyle(.plain).accessibilityLabel("Search your music library")
                    #if os(iOS)
                    .submitLabel(.search)
                    .autocorrectionDisabled()
                    #endif
                    .onSubmit { model.search(search) }
                if !search.isEmpty {
                    Button { search = "" } label: { Image(systemName: "xmark.circle.fill").frame(width: 44, height: 44) }
                        .buttonStyle(.plain).foregroundStyle(.secondary).accessibilityLabel("Clear search")
                }
            }.padding(.horizontal, 16).frame(minHeight: 52)
                .background(RemoteStyle.surface, in: RoundedRectangle(cornerRadius: 16))
            if !model.query.isEmpty {
                Text("Results for “\(model.query)”").font(.subheadline).foregroundStyle(.secondary)
            }
            LazyVStack(alignment: .leading, spacing: 12) {
                ForEach(Array(model.hits.enumerated()), id: \.offset) { _, hit in
                    VStack(alignment: .leading, spacing: 12) {
                        HStack(alignment: .top, spacing: 12) {
                            Image(systemName: hit.kind == "album" ? "square.stack" : "music.note")
                                .font(.title3).foregroundStyle(RemoteStyle.accent).frame(width: 28).accessibilityHidden(true)
                            VStack(alignment: .leading, spacing: 5) {
                                Text(hit.title).font(.headline)
                                if let subtitle = hit.subtitle { Text(subtitle).font(.subheadline).foregroundStyle(.secondary) }
                            }
                        }
                        if let uri = hit.sourceUri { sourceButtons(uri) }
                        else if let favorite = hit.favorite {
                            Button("Play favorite", systemImage: "play.fill") { model.prepare(.playFavorite(favorite)) }
                                .buttonStyle(.bordered).disabled(!ready)
                        }
                    }.remoteCard()
                }
            }
            if !model.panelLoading && model.hits.isEmpty && model.panelProblem == nil {
                EmptyMusicState(title: model.query.isEmpty ? "What are you in the mood for?" : "No matching music",
                                systemImage: "magnifyingglass",
                                detail: model.query.isEmpty ? "Search the music saved in your library by artist, album or track." : "Try a different title or artist. Results come from the library cached by your daemon.")
            }
        }
        .onAppear { search = model.query }
        .task(id: search) {
            guard model.panel == .library && search.trimmingCharacters(in: .whitespacesAndNewlines) != model.query else { return }
            try? await Task.sleep(for: .milliseconds(350))
            guard !Task.isCancelled && model.panel == .library else { return }
            model.search(search)
        }
    }

    private var sourceEntry: some View {
        DisclosureGroup {
            VStack(alignment: .leading, spacing: 12) {
                TextField("Spotify link or supported source URI", text: $source)
                    .textFieldStyle(.roundedBorder).accessibilityLabel("Music source link")
                    #if os(iOS)
                    .keyboardType(.URL)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    #endif
                sourceButtons(source)
                Text("Play changes this group’s source. Queue adds music without starting playback.")
                    .font(.caption).foregroundStyle(.secondary)
            }.padding(.top, 12)
        } label: { Label("Play a link", systemImage: "link").font(.headline).frame(minHeight: 32) }
            .remoteCard()
    }

    private func sourceButtons(_ uri: String) -> some View {
        HStack(spacing: 12) {
            Button("Play now", systemImage: "play.fill") { model.prepare(.playSource(uri)) }
                .buttonStyle(.borderedProminent).foregroundStyle(RemoteStyle.accentInk)
            Menu {
                Button("Queue next", systemImage: "text.line.first.and.arrowtriangle.forward") { model.prepare(.queueAdd(uri, next: true)) }
                Button("Add to end of queue", systemImage: "text.append") { model.prepare(.queueAdd(uri, next: false)) }
            } label: { Label("Queue", systemImage: "text.badge.plus") }
                .buttonStyle(.bordered)
        }
        .disabled(!ready || uri.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
        .accessibilityHint("Opens a review before changing the selected group.")
    }
}

/// Selection shares the visible queue's reads; it owns no transport or timer.
@MainActor private struct QueueBlocksPanel: View {
    @ObservedObject var bridge: RemoteBridge
    @Binding var selecting: Bool
    let refreshID: UUID
    @State private var selection = QueueSelection()
    @State private var problem: String?
    private var remote: RemoteModel { bridge.model }
    private var ready: Bool {
        remote.canPrepare && remote.panel == .queue && !remote.panelLoading && remote.house?.isConnected == true
    }

    var body: some View {
        content
            .onAppear { observePage() }
            .onDisappear { selection.clear(); selecting = false; problem = nil }
            .onChange(of: remote.queue) { _, _ in observePage() }
            .onChange(of: remote.panelLoading) { _, loading in if !loading { observePage() } }
            .onChange(of: remote.selectedZone) { _, zone in
                if let previous = selection.zone, zone?.sameGroup(as: previous) != true { retire() }
            }
            .onChange(of: remote.active) { _, active in if !active { retire(); selecting = false } }
            .onChange(of: remote.house?.isConnected) { _, _ in retire() }
            .onChange(of: remote.house?.panelRevision) { _, _ in retire() }
            .onChange(of: remote.panelProblem) { _, value in if value != nil { retire() } }
            .onChange(of: refreshID) { _, _ in retire() }
    }

    private var content: some View {
        VStack(alignment: .leading, spacing: 12) {
            Button(selecting ? "Done selecting" : "Select entries to move together",
                   systemImage: selecting ? "checkmark.circle" : "checklist") {
                if selecting { selection.clear(); selecting = false; problem = nil }
                else if ready, let page = remote.queue, !page.items.isEmpty {
                    selecting = true; problem = nil; observePage()
                }
            }.buttonStyle(.bordered)
                .disabled(!selecting && (!ready || remote.queue?.items.isEmpty != false || remote.queue?.revision == nil))
            if selecting {
                Text("Select up to 100 consecutive entries. Previous and Next keep selected entries across pages of the same queue. Nothing plays while you select.")
                    .font(.callout).foregroundStyle(.secondary)
                Text("\(selection.items.count) of 100 selected").font(.subheadline.bold())
                if !selection.isEmpty && !selection.isContiguous {
                    Label("The selection has gaps. Select every intended entry in one consecutive block, or deselect the separate entries.", systemImage: "info.circle")
                        .font(.caption).foregroundStyle(.secondary)
                }
                if let problem {
                    Label(problem, systemImage: "exclamationmark.circle").font(.callout).textSelection(.enabled)
                }
                if let page = remote.queue, let zone = remote.selectedZone {
                    actions(page, zone: zone, offset: remote.queueOffset)
                    LazyVStack(alignment: .leading, spacing: 10) {
                        ForEach(page.items) { item in selectionRow(item, page: page, zone: zone, offset: remote.queueOffset) }
                    }
                    if page.items.isEmpty { Text("This page has no entries. Return to the previous page or refresh.").font(.callout) }
                }
                Text("A move preserves the block’s internal order and does not start or restart playback. Review the full range before confirming; queue moves cannot be undone.")
                    .font(.caption).foregroundStyle(.secondary)
            }
        }.remoteCard()
    }

    private func actions(_ page: QueuePage, zone: Zone, offset: UInt32) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            ViewThatFits(in: .horizontal) {
                HStack { selectionButtons(page, zone: zone, offset: offset) }
                VStack(alignment: .leading) { selectionButtons(page, zone: zone, offset: offset) }
            }
            blockMoveMenu(page, zone: zone)
        }
    }

    @ViewBuilder private func selectionButtons(_ page: QueuePage, zone: Zone, offset: UInt32) -> some View {
        Button("Select this page", systemImage: "checkmark.square") {
            guard isVisible(page, zone: zone, offset: offset) else { return }
            do { try selection.selectPage(page, in: zone, offset: offset); problem = nil }
            catch { problem = error.localizedDescription }
        }.buttonStyle(.bordered).disabled(!ready || page.items.isEmpty)
        Button("Clear selection", systemImage: "xmark.circle") { selection.clear(); problem = nil }
            .buttonStyle(.bordered).disabled(selection.isEmpty)
    }

    private func selectionRow(_ item: QueueItem, page: QueuePage, zone: Zone, offset: UInt32) -> some View {
        let chosen = selection.contains(item)
        return Button {
            guard isVisible(page, zone: zone, offset: offset) else { return }
            do { try selection.toggle(item, in: page, zone: zone, offset: offset); problem = nil }
            catch { problem = error.localizedDescription }
        } label: {
            HStack(alignment: .top, spacing: 12) {
                Image(systemName: chosen ? "checkmark.circle.fill" : "circle")
                    .font(.title2).foregroundStyle(RemoteStyle.accent).frame(width: 32, height: 44)
                VStack(alignment: .leading, spacing: 4) {
                    Text("\(item.position). \(item.title)").font(.headline).foregroundStyle(.primary)
                    Text([item.creator, item.album].compactMap { $0 }.joined(separator: " · "))
                        .font(.caption).foregroundStyle(.secondary)
                    if page.playing == item.position { Text("Now playing").font(.caption).foregroundStyle(RemoteStyle.accent) }
                    if !QueueSelection.canSelect(item) { Text("Queue identity unavailable").font(.caption).foregroundStyle(.secondary) }
                }.frame(maxWidth: .infinity, alignment: .leading)
            }.multilineTextAlignment(.leading).frame(minHeight: 44).contentShape(Rectangle())
        }.buttonStyle(.plain).disabled(!ready || !QueueSelection.canSelect(item))
            .accessibilityValue(chosen ? "Selected" : "Not selected")
            .accessibilityHint("Changes only the local selection. Does not play this entry.")
    }

    private func blockMoveMenu(_ page: QueuePage, zone: Zone) -> some View {
        let token = selection.id
        return Menu("Review moving selected block", systemImage: "arrow.up.arrow.down") {
            ForEach(QueueMoveDestination.allCases, id: \.self) { destination in
                let move = try? selection.move(to: destination)
                Button(destination.title) {
                    guard selecting, ready, remote.queue == page,
                          remote.selectedZone?.sameGroup(as: zone) == true,
                          selection.isCurrent(token, page: page, zone: zone), let move else { return }
                    // The complete immutable block enters the same one-use
                    // review as single-entry moves, never a loop of mutations.
                    remote.prepare(.queueMove(move), in: zone)
                    selection.clear(); selecting = false; problem = nil
                }.disabled(move == nil)
            }
        }.buttonStyle(.borderedProminent).foregroundStyle(RemoteStyle.accentInk)
            .disabled(!ready || !selection.isContiguous || !selection.isCurrent(token, page: page, zone: zone))
    }

    private func isVisible(_ page: QueuePage, zone: Zone, offset: UInt32) -> Bool {
        selecting && ready && remote.queue == page && remote.queueOffset == offset &&
            remote.selectedZone?.sameGroup(as: zone) == true
    }

    private func observePage() {
        // A page switch briefly clears remote.queue. Keep only the bounded
        // selected occurrences while the shared read slot fetches the new page.
        guard selecting, remote.active, remote.panel == .queue, !remote.panelLoading,
              let page = remote.queue, let zone = remote.selectedZone else { return }
        let hadSelection = !selection.isEmpty
        do {
            try selection.observe(page, in: zone, offset: remote.queueOffset)
            if hadSelection && selection.isEmpty { problem = "The queue or group changed. Select entries again." }
        } catch { problem = error.localizedDescription }
    }

    private func retire() {
        if !selection.isEmpty { problem = "The queue or connection was invalidated. Select entries again after it refreshes." }
        selection.clear()
    }
}

/// Results stay separate from ordinary queue pages: matches can be far apart.
@MainActor private struct QueueSearchPanel: View {
    @ObservedObject var bridge: RemoteBridge
    @State private var expanded = false
    @State private var selectionProblem: String?
    private var search: QueueSearchModel { bridge.queueSearch }
    private var remote: RemoteModel { bridge.model }
    private var visible: Bool { expanded && remote.active && remote.panel == .queue }

    var body: some View {
        DisclosureGroup(isExpanded: $expanded) {
            VStack(alignment: .leading, spacing: 14) {
                Text("Search titles, artists and albums already in this group’s queue, including beyond the visible page. Typing does not send a request.")
                    .font(.callout).foregroundStyle(.secondary)
                TextField("Words from a title, artist or album", text: Binding(
                    get: { search.query }, set: { search.edit($0); selectionProblem = nil }))
                    .textFieldStyle(.roundedBorder).accessibilityLabel("Search this queue")
                    .disabled(!visible)
                    #if os(iOS)
                    .submitLabel(.search)
                    .autocorrectionDisabled()
                    #endif
                    .onSubmit { if visible { search.search() } }
                controls
                if let problem = search.problem ?? selectionProblem {
                    Label(problem, systemImage: "exclamationmark.circle")
                        .font(.callout).textSelection(.enabled)
                }
                if let observation = search.observation { results(observation) }
            }.padding(.top, 12)
        } label: {
            Label("Find music in this queue", systemImage: "magnifyingglass")
                .font(.headline).frame(minHeight: 44)
        }
        .remoteCard()
        .onAppear { synchronize() }
        .onDisappear { search.suspend() }
        .onChange(of: expanded) { _, _ in synchronize() }
        .onChange(of: remote.active) { _, _ in synchronize() }
        .onChange(of: remote.selectedZone) { _, _ in synchronize() }
        .onChange(of: remote.house?.panelRevision) { _, _ in search.invalidate() }
        .onChange(of: remote.house?.isConnected) { _, _ in search.invalidate() }
        .onChange(of: remote.queue?.revision) { before, after in
            // Ignore a repeated observation of the same queue. A new revision,
            // including a failed page read, retires results without rescanning.
            if before != after { search.invalidate() }
        }
    }

    private var controls: some View {
        VStack(alignment: .leading, spacing: 10) {
            if search.loading {
                ProgressView("Searching this section…")
                Button("Cancel queue search", systemImage: "xmark.circle") { search.cancel() }
                    .buttonStyle(.bordered)
                Text("A cancelled read must finish draining before another search can start.")
                    .font(.caption).foregroundStyle(.secondary)
            } else {
                Button(search.observation == nil ? "Search queue" : "Search from start", systemImage: "magnifyingglass") {
                    guard visible else { return }
                    selectionProblem = nil; search.search()
                }.buttonStyle(.bordered).disabled(!search.canSearch)
                if search.canContinue {
                    Button("Search next section", systemImage: "chevron.right") {
                        guard visible else { return }
                        selectionProblem = nil; search.continueSearch()
                    }.buttonStyle(.bordered)
                }
            }
        }
    }

    private func results(_ observation: QueueSearchObservation) -> some View {
        let page = observation.page
        return VStack(alignment: .leading, spacing: 12) {
            Text(search.isCurrent ? "Search results" : "Previous results — search again before acting")
                .font(.headline)
            Text("\(observation.request.zone.title) · \(observation.request.zone.household)")
                .font(.subheadline)
            Text("For “\(observation.request.query)” · \(page.items.count) matches in this section")
                .font(.subheadline)
            Text(page.coverage).font(.caption).foregroundStyle(.secondary)
            Text(page.complete ? "End of queue reached. Earlier result batches are not retained."
                 : "More of the queue remains. Continue explicitly to inspect the next section.")
                .font(.caption).foregroundStyle(.secondary)
            if page.items.isEmpty {
                Text("No matches in the examined section.").font(.callout)
            }
            LazyVStack(alignment: .leading, spacing: 12) {
                ForEach(page.items) { item in resultRow(item, observation: observation) }
            }
        }
    }

    private func resultRow(_ item: QueueItem, observation: QueueSearchObservation) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("\(item.position). \(item.title)").font(.headline)
            Text([item.creator, item.album].compactMap { $0 }.joined(separator: " · "))
                .font(.subheadline).foregroundStyle(.secondary)
            if item.playable {
                Button("Review playing this occurrence", systemImage: "play.circle") {
                    guard visible, remote.canPrepare else { return }
                    do {
                        let selected = try search.selection(item, from: observation.id)
                        guard remote.selectedZone?.sameGroup(as: selected.zone) == true else {
                            throw ContractError.invalid("The selected group changed. Search its queue again.")
                        }
                        selectionProblem = nil
                        remote.prepare(selected.command, in: selected.zone)
                    } catch { selectionProblem = error.localizedDescription }
                }.buttonStyle(.bordered)
                    .disabled(!visible || !search.isCurrent || search.loading || !remote.canPrepare)
                    .accessibilityHint("Reviews this exact queue position and revision before playback. It does not add a duplicate track.")
            } else {
                Text("No usable renderer identity; this occurrence is informational only.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            Divider()
        }
    }

    private func synchronize() {
        if visible { search.activate(in: remote.selectedZone) }
        else { search.suspend() }
        selectionProblem = nil
    }
}
#endif
