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
                    else { model.refreshPanel() }
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
            case .dj: djPanel
            case .library: libraryPanel
            case .scenes: ScenePanel(bridge: bridge)
            case .sleep: SleepPanel(bridge: bridge)
            case .schedules: SchedulesPanel(bridge: bridge)
            }
        }
    }

    @ViewBuilder private var queuePanel: some View {
        if let page = model.queue {
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
        sourceEntry
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

    @ViewBuilder private var favoritesPanel: some View {
        Text("The music and stations saved to this household.")
            .font(.subheadline).foregroundStyle(.secondary)
        if model.favorites.isEmpty && !model.panelLoading && model.panelProblem == nil {
            EmptyMusicState(title: "Keep your favorites close", systemImage: "heart",
                            detail: "Favorites saved in Sonos appear here for this household. Refresh after adding one.")
        }
        LazyVStack(spacing: 12) {
            ForEach(model.favorites) { favorite in
                Button { model.prepare(.playFavorite(favorite.id)) } label: {
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
                    }.remoteCard()
                }.buttonStyle(.plain).disabled(!ready || !favorite.playable)
                    .accessibilityHint("Review playing this favorite in the selected group.")
            }
        }
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
#endif
