#if os(macOS) || os(iOS)
import SwiftUI
import FrankenSonosKit

/// A destination-independent library. All playback still enters RemoteModel's
/// existing review sheet with the explicitly selected household and group.
@MainActor struct LibraryBrowserPanel: View {
    @ObservedObject var bridge: RemoteBridge
    @Environment(\.scenePhase) private var scenePhase
    @Environment(\.dynamicTypeSize) private var typeSize
    @State private var filter = ""
    @State private var actionProblem: String?
    private var catalog: LibraryCatalogModel { bridge.catalog }
    private var remote: RemoteModel { bridge.model }
    private var canPlay: Bool { catalog.canChoose && remote.canPrepare }
    private var validFilter: Bool { (try? LibraryBrowseRequest(query: filter)) != nil }

    var body: some View {
        Group {
            if catalog.playlist.isOpen { PlaylistContentsPanel(bridge: bridge) }
            else { catalogBody }
        }
        .onAppear {
            filter = catalog.request.query
            if scenePhase == .active { resume() }
        }
        .onDisappear { catalog.suspend() }
        .onChange(of: scenePhase) { _, phase in
            if phase == .active { resume() }
            else if phase == .background { catalog.suspend() }
        }
        .onChange(of: remote.active) { _, active in
            if !active { catalog.suspend() }
            else if scenePhase == .active { resume() }
        }
        .onChange(of: filter) { _, value in
            actionProblem = nil
            catalog.filter(value)
        }
        .onChange(of: catalog.request.query) { _, value in
            if filter.trimmingCharacters(in: .whitespacesAndNewlines) != value { filter = value }
        }
    }

    private var catalogBody: some View {
        VStack(alignment: .leading, spacing: 18) {
            if let album = catalog.album { albumHeader(album) }
            else {
                Picker("Browse your library", selection: Binding(
                    get: { catalog.request.category },
                    set: { actionProblem = nil; catalog.browse($0); filter = catalog.request.query }
                )) {
                    ForEach([LibraryCategory.albums, .tracks, .playlists], id: \.self) {
                        Text($0.title).tag($0)
                    }
                }.pickerStyle(.segmented)
            }

            filterField
            if remote.selectedZone == nil {
                Label("Choose a room above when you’re ready to play or queue music.", systemImage: "hifispeaker")
                    .font(.callout).foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }

            if let problem = catalog.problem {
                VStack(alignment: .leading, spacing: 12) {
                    Label(!validFilter ? "Check your library filter" : catalog.restartRequired ? "Your library has changed" : "Couldn’t read this library page",
                          systemImage: "exclamationmark.circle").font(.headline)
                    Text(problem).font(.callout).foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true).textSelection(.enabled)
                    if !validFilter {
                        Button("Clear filter", systemImage: "xmark") { filter = "" }.buttonStyle(.bordered)
                    } else {
                        Button(catalog.restartRequired ? "Refresh library" : "Try again", systemImage: "arrow.clockwise") {
                            catalog.refresh()
                        }.buttonStyle(.bordered).disabled(!catalog.canRefresh)
                    }
                }.remoteCard()
            }
            if let actionProblem {
                Label(actionProblem, systemImage: "exclamationmark.circle")
                    .font(.callout).foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true).remoteCard()
            }

            if let page = catalog.page {
                pageSummary(page)
                if page.items.isEmpty && catalog.isFresh { emptyState }
                else if page.kind == .albums {
                    LazyVGrid(columns: typeSize.isAccessibilitySize ? [GridItem(.flexible())] : [GridItem(.adaptive(minimum: 150), spacing: 14)],
                              alignment: .leading, spacing: 14) {
                        ForEach(page.items) { albumCard($0) }
                    }
                } else {
                    LazyVStack(spacing: 10) {
                        ForEach(page.items) { item in
                            musicRow(item)
                            if item.kind == "playlist" {
                                let selection = catalog.selection(for: item.id)
                                Button("View cached tracks", systemImage: "list.bullet") {
                                    guard let selection else { return }
                                    do { try catalog.openPlaylist(selection); actionProblem = nil }
                                    catch { actionProblem = error.localizedDescription }
                                }.buttonStyle(.bordered).disabled(selection == nil)
                                    .accessibilityLabel("View cached tracks in \(item.title)")
                                    .accessibilityHint("Reads playlist contents without playing or syncing music.")
                            }
                        }
                    }
                }
                if page.total > LibraryBrowseRequest.pageSize || page.offset > 0 { pagination(page) }
            } else if catalog.loading {
                ProgressView(catalog.album == nil ? "Opening your library…" : "Loading album tracks…")
                    .frame(maxWidth: .infinity, minHeight: 180).accessibilityLabel("Loading library")
            }

            Text("Showing the daemon’s cached music, which can include retired or taste-derived items. Album listings show saved-library counts; open an album to see whether its full playable track list has been cached. Manage Spotify and sync in House → Spotify & library.")
                .font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
        }
    }

    private var filterField: some View {
        HStack(spacing: 10) {
            Image(systemName: "magnifyingglass").foregroundStyle(.secondary).accessibilityHidden(true)
            TextField(catalog.album == nil ? "Filter \(catalog.request.category.title.lowercased())" : "Find a track on this album",
                      text: $filter)
                .textFieldStyle(.plain).accessibilityLabel("Filter this library listing")
                #if os(iOS)
                .submitLabel(.search).autocorrectionDisabled()
                #endif
            if !filter.isEmpty {
                Button { filter = "" } label: {
                    Image(systemName: "xmark.circle.fill").frame(width: 44, height: 44)
                }.buttonStyle(.plain).foregroundStyle(.secondary).accessibilityLabel("Clear library filter")
            }
        }.padding(.horizontal, 16).frame(minHeight: 52)
            .background(RemoteStyle.surface, in: RoundedRectangle(cornerRadius: 16))
    }

    private func albumHeader(_ album: LibraryCatalogItem) -> some View {
        VStack(alignment: .leading, spacing: 16) {
            Button { actionProblem = nil; catalog.backToAlbums(); filter = catalog.request.query } label: {
                Label("Back to albums", systemImage: "chevron.left").frame(minHeight: 32)
            }.buttonStyle(.plain).foregroundStyle(RemoteStyle.accent)
            HStack(alignment: .top, spacing: 16) {
                Image(systemName: "square.stack.fill").font(.system(size: 32, weight: .medium))
                    .foregroundStyle(RemoteStyle.accent).frame(width: 80, height: 80)
                    .background(RemoteStyle.accent.opacity(0.10), in: RoundedRectangle(cornerRadius: 20))
                    .accessibilityHidden(true)
                VStack(alignment: .leading, spacing: 6) {
                    Text(album.title).font(.title2.bold()).fixedSize(horizontal: false, vertical: true)
                        .accessibilityAddTraits(.isHeader)
                    if let subtitle = album.subtitle, !subtitle.isEmpty {
                        Text(subtitle).font(.subheadline).foregroundStyle(.secondary)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                    if album.explicit { explicitBadge }
                }
            }
            ViewThatFits(in: .horizontal) {
                HStack(spacing: 12) { albumActions }
                VStack(alignment: .leading, spacing: 12) { albumActions }
            }
            Text("Play album and Queue album use the full album source. The tracks below follow cached disc/track order.")
                .font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            if let observation = catalog.page?.albumObservation {
                Text(observation.description).font(.callout).foregroundStyle(.secondary)
                if let fetchedAt = observation.fetchedAt {
                    Text("Album cached " + Date(timeIntervalSince1970: TimeInterval(fetchedAt))
                        .formatted(date: .abbreviated, time: .shortened))
                        .font(.caption).foregroundStyle(.secondary)
                }
            } else {
                Text("Only the saved-library subset is known; some movements or tracks may be missing.")
                    .font(.caption).foregroundStyle(.secondary)
            }
        }.remoteCard()
    }

    @ViewBuilder private var albumActions: some View {
        let selection = catalog.albumSelection()
        let zone = remote.selectedZone
        Button("Play album", systemImage: "play.fill") {
            if let selection { chooseAlbum(selection, action: .playNow, in: zone) }
        }
            .buttonStyle(.borderedProminent).foregroundStyle(RemoteStyle.accentInk)
            .disabled(selection == nil || !remote.canPrepare)
            .accessibilityHint("Review playing this album in the selected group.")
        Menu {
            if let selection {
                Button("Queue next", systemImage: "text.line.first.and.arrowtriangle.forward") { chooseAlbum(selection, action: .queueNext, in: zone) }
                Button("Add to end of queue", systemImage: "text.append") { chooseAlbum(selection, action: .queueLast, in: zone) }
            }
        } label: { Label("Queue album", systemImage: "text.badge.plus") }
            .buttonStyle(.bordered).disabled(selection == nil || !remote.canPrepare)
    }

    private func pageSummary(_ page: LibraryCatalogPage) -> some View {
        ViewThatFits(in: .horizontal) {
            HStack(alignment: .firstTextBaseline) {
                countLabel(page)
                Spacer(minLength: 8)
                freshnessLabel
            }
            VStack(alignment: .leading, spacing: 6) {
                countLabel(page)
                freshnessLabel
            }
        }
    }

    private func countLabel(_ page: LibraryCatalogPage) -> some View {
        Text("\(page.total) \(page.kind.title.lowercased())\(catalog.request.query.isEmpty ? (page.albumUri == nil ? " in your library" : " on this album") : " found")")
            .font(.subheadline.weight(.medium)).accessibilityAddTraits(.isHeader)
    }

    @ViewBuilder private var freshnessLabel: some View {
        if catalog.loading {
            HStack(spacing: 6) { ProgressView().controlSize(.small); Text("Updating…") }
                .font(.caption).foregroundStyle(.secondary).accessibilityElement(children: .combine)
        } else if let observedAt = catalog.observedAt {
            HStack(spacing: 4) {
                Text(catalog.isFresh ? "Read" : "Last read")
                Text(observedAt, style: .relative)
                Text("ago")
            }.font(.caption).foregroundStyle(.secondary).accessibilityElement(children: .combine)
        }
    }

    private func albumCard(_ item: LibraryCatalogItem) -> some View {
        let selection = catalog.selection(for: item.id)
        let zone = remote.selectedZone
        return VStack(alignment: .leading, spacing: 8) {
            Button {
                if let selection {
                    actionProblem = nil; catalog.openAlbum(selection); filter = catalog.request.query
                }
            } label: {
                VStack(alignment: .leading, spacing: 12) {
                    Image(systemName: "square.stack.fill")
                        .font(.system(size: 34, weight: .medium)).foregroundStyle(RemoteStyle.accent)
                        .frame(maxWidth: .infinity).frame(height: 100)
                        .background(RemoteStyle.accent.opacity(0.09), in: RoundedRectangle(cornerRadius: 14))
                        .accessibilityHidden(true)
                    Text(item.title).font(.headline).foregroundStyle(.primary).lineLimit(typeSize.isAccessibilitySize ? nil : 3)
                        .frame(maxWidth: .infinity, alignment: .leading)
                    if let subtitle = item.subtitle, !subtitle.isEmpty {
                        Text(subtitle).font(.subheadline).foregroundStyle(.secondary).lineLimit(typeSize.isAccessibilitySize ? nil : 2)
                    }
                }.multilineTextAlignment(.leading).contentShape(Rectangle())
            }.buttonStyle(.plain).disabled(selection == nil)
                .accessibilityHint("Browse cached tracks on this album. The list may be incomplete.")
            HStack(spacing: 6) {
                if item.explicit { explicitBadge }
                Text("\(item.cachedTracks ?? 0) cached tracks").font(.caption).foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                Spacer(minLength: 0)
                itemMenu(selection, title: item.title, in: zone)
            }
        }.padding(12).frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
            .background(RemoteStyle.surface, in: RoundedRectangle(cornerRadius: 20))
    }

    private func musicRow(_ item: LibraryCatalogItem) -> some View {
        let selection = catalog.selection(for: item.id)
        let zone = remote.selectedZone
        return HStack(spacing: 10) {
            Button {
                if let selection { choose(selection, action: .playNow, in: zone) }
            } label: {
                HStack(spacing: 12) {
                    if catalog.album != nil, let track = item.trackNumber {
                        VStack(spacing: 3) {
                            Text("\(track)").font(.subheadline.monospacedDigit())
                            if let disc = item.discNumber, disc > 1 { Text("Disc \(disc)").font(.caption2) }
                        }.foregroundStyle(.secondary).frame(width: 42).accessibilityLabel(trackLabel(item))
                    } else {
                        Image(systemName: item.kind == "playlist" ? "music.note.list" : "music.note")
                            .font(.title3).foregroundStyle(RemoteStyle.accent).frame(width: 46, height: 46)
                            .background(RemoteStyle.accent.opacity(0.08), in: RoundedRectangle(cornerRadius: 12))
                            .accessibilityHidden(true)
                    }
                    VStack(alignment: .leading, spacing: 5) {
                        Text(item.title).font(.headline).foregroundStyle(.primary)
                        if let subtitle = item.subtitle, !subtitle.isEmpty {
                            Text(subtitle).font(.subheadline).foregroundStyle(.secondary)
                        }
                        if item.explicit { explicitBadge }
                        if !item.explicitKnown {
                            Text("Explicitness unknown").font(.caption).foregroundStyle(.secondary)
                        }
                    }.frame(maxWidth: .infinity, alignment: .leading).multilineTextAlignment(.leading)
                    Image(systemName: "play.fill").font(.caption).foregroundStyle(RemoteStyle.accent)
                        .accessibilityHidden(true)
                }.frame(minHeight: 44).contentShape(Rectangle())
            }.buttonStyle(.plain).disabled(!canPlay || selection == nil)
                .accessibilityHint("Review playing this \(item.kind) in the selected group.")
            itemMenu(selection, title: item.title, in: zone)
        }.padding(12).background(RemoteStyle.surface, in: RoundedRectangle(cornerRadius: 18))
    }

    private func itemMenu(_ selection: LibraryCatalogSelection?, title: String, in zone: Zone?) -> some View {
        Menu {
            if let selection {
                Button("Play now", systemImage: "play") { choose(selection, action: .playNow, in: zone) }
                Button("Queue next", systemImage: "text.line.first.and.arrowtriangle.forward") { choose(selection, action: .queueNext, in: zone) }
                Button("Add to end of queue", systemImage: "text.append") { choose(selection, action: .queueLast, in: zone) }
            }
        } label: { Image(systemName: "ellipsis").frame(width: 44, height: 44) }
            .disabled(!canPlay || selection == nil).accessibilityLabel("Options for \(title)")
    }

    private var explicitBadge: some View {
        Text("E").font(.caption2.bold()).foregroundStyle(.secondary)
            .padding(.horizontal, 5).padding(.vertical, 2)
            .background(.secondary.opacity(0.12), in: RoundedRectangle(cornerRadius: 4))
            .accessibilityLabel("Explicit")
    }

    private func pagination(_ page: LibraryCatalogPage) -> some View {
        VStack(spacing: 10) {
            if !page.items.isEmpty {
                Text("\(UInt64(page.offset) + 1)–\(UInt64(page.offset) + UInt64(page.items.count)) of \(page.total)")
                    .font(.caption.monospacedDigit()).foregroundStyle(.secondary)
            }
            HStack {
                Button { actionProblem = nil; catalog.previousPage() } label: {
                    Label("Previous", systemImage: "chevron.left").frame(minHeight: 32)
                }.disabled(!catalog.canGoBack)
                Spacer(minLength: 8)
                Button { actionProblem = nil; catalog.nextPage() } label: {
                    Label("Next", systemImage: "chevron.right").frame(minHeight: 32)
                }.disabled(!catalog.canGoForward)
            }.buttonStyle(.bordered)
        }.padding(.vertical, 6)
    }

    private var emptyState: some View {
        EmptyMusicState(title: catalog.request.query.isEmpty ? "Your music belongs here" : "No matching \(catalog.request.category.title.lowercased())",
                        systemImage: catalog.request.category == .albums ? "square.stack" : "music.note.list",
                        detail: catalog.request.query.isEmpty
                            ? "Sync your saved Spotify music in House → Spotify & library, then refresh this listing."
                            : "Try another title or artist, or clear the filter to see everything in this listing.")
    }

    private func trackLabel(_ item: LibraryCatalogItem) -> String {
        if let disc = item.discNumber { return "Disc \(disc), track \(item.trackNumber ?? 0)" }
        return "Track \(item.trackNumber ?? 0)"
    }

    private func choose(_ selection: LibraryCatalogSelection, action: LibraryCatalogAction, in zone: Zone?) {
        prepare(in: zone) { try catalog.command(for: selection, action: action) }
    }

    private func resume() {
        guard remote.active else { return }
        catalog.resume()
        // A removed view discards its draft. Recover the displayed valid filter
        // if an invalid draft had suspended the retained browser model.
        catalog.filter(filter)
    }

    private func chooseAlbum(_ selection: LibraryCatalogSelection, action: LibraryCatalogAction, in zone: Zone?) {
        prepare(in: zone) { try catalog.command(forAlbum: selection, action: action) }
    }

    private func prepare(in expectedZone: Zone?, _ command: () throws -> NativeCommand) {
        do {
            guard remote.canPrepare, let zone = expectedZone, remote.selectedZone?.sameGroup(as: zone) == true else {
                throw ContractError.invalid("The destination changed or another action is active. Choose the music again for the displayed group.")
            }
            let selected = try command()
            actionProblem = nil
            remote.prepare(selected, in: zone)
        } catch { actionProblem = error.localizedDescription }
    }
}

/// Playlist positions identify cached source occurrences, never queue slots.
@MainActor private struct PlaylistContentsPanel: View {
    @ObservedObject var bridge: RemoteBridge
    @State private var actionProblem: String?
    private var catalog: LibraryCatalogModel { bridge.catalog }
    private var detail: PlaylistContentsModel { catalog.playlist }
    private var remote: RemoteModel { bridge.model }

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Button("Back to playlists", systemImage: "chevron.left") { catalog.backToPlaylists() }
                .buttonStyle(.bordered).disabled(!catalog.active)
            Text(detail.name).font(.title2.bold()).accessibilityAddTraits(.isHeader)
            Text("Cached tracks in original playlist order. Opening or filtering this list does not play music or sync Spotify.")
                .font(.callout).foregroundStyle(.secondary)
            TextField("Find a title, artist or album in this playlist", text: Binding(
                get: { detail.query }, set: { detail.edit($0); actionProblem = nil }))
                .textFieldStyle(.roundedBorder).accessibilityLabel("Filter cached playlist tracks")
                .disabled(!detail.active)
                #if os(iOS)
                .submitLabel(.search).autocorrectionDisabled()
                #endif
                .onSubmit { detail.refresh() }
            if detail.query.trimmingCharacters(in: .whitespacesAndNewlines).unicodeScalars.count > 512 {
                Text("Use at most 512 Unicode characters.").font(.caption).foregroundStyle(.red)
            }
            if detail.loading {
                ProgressView("Reading cached playlist…")
                Button("Cancel read", systemImage: "xmark.circle") { detail.cancel() }.buttonStyle(.bordered)
            } else {
                Button("Read from start / apply filter", systemImage: "arrow.clockwise") { detail.refresh() }
                    .buttonStyle(.bordered).disabled(!detail.canRefresh)
            }
            if let problem = actionProblem ?? detail.problem {
                Label(problem, systemImage: "exclamationmark.circle")
                    .font(.callout).textSelection(.enabled).remoteCard()
            }
            if let observed = detail.observation { contents(observed) }
            if remote.selectedZone == nil {
                Text("Browsing does not need a speaker. Choose a group above before playing or queueing a track.")
                    .font(.callout).foregroundStyle(.secondary)
            }
        }
    }

    private func contents(_ observed: PlaylistContentsObservation) -> some View {
        let page = observed.page
        return VStack(alignment: .leading, spacing: 14) {
            if !detail.isCurrent {
                Label("Previous page — read again before acting", systemImage: "clock.arrow.circlepath")
                    .font(.headline)
            }
            Text(page.coverageText).font(.callout).foregroundStyle(.secondary)
            if let at = page.fetchedAt {
                Text(cacheTime(at)).font(.caption).foregroundStyle(.secondary)
            }
            Text("Filter: \(observed.request.query.isEmpty ? "all cached tracks" : observed.request.query)")
                .font(.caption)
            if page.items.isEmpty { Text(page.emptyText).font(.callout) }
            else {
                Text("Showing cached matches \(UInt64(page.offset) + 1)–\(UInt64(page.offset) + UInt64(page.items.count)) of \(page.total)")
                    .font(.caption.monospacedDigit()).foregroundStyle(.secondary)
                LazyVStack(alignment: .leading, spacing: 12) {
                    ForEach(page.items) { item in track(item, observed: observed) }
                }
            }
            HStack {
                Button("Previous", systemImage: "chevron.left") { detail.previousPage() }
                    .disabled(!detail.canGoBack)
                Spacer(minLength: 8)
                Button("Next", systemImage: "chevron.right") { detail.nextPage() }
                    .disabled(!detail.canGoForward)
            }.buttonStyle(.bordered)
            Text("Only this page is retained. Gaps are unavailable source entries; repeated tracks remain separate occurrences. Play selects one track, not the rest of the playlist.")
                .font(.caption).foregroundStyle(.secondary)
        }
    }

    private func track(_ item: PlaylistTrack, observed: PlaylistContentsObservation) -> some View {
        let zone = remote.selectedZone
        return VStack(alignment: .leading, spacing: 8) {
            Text("\(item.position). \(item.title)").font(.headline)
            Text([item.subtitle, item.album ?? ""].filter { !$0.isEmpty }.joined(separator: " · "))
                .font(.subheadline).foregroundStyle(.secondary)
            HStack(spacing: 10) {
                if item.explicit { Text("Explicit").font(.caption.weight(.semibold)) }
                if let seconds = item.durationSecs {
                    Text("\(seconds / 60):\(String(format: "%02d", seconds % 60))").font(.caption.monospacedDigit())
                }
            }.foregroundStyle(.secondary)
            Menu {
                ForEach(PlaylistTrackSelection.Action.allCases, id: \.self) { action in
                    Button(action.title) { choose(item, observed: observed, action: action, in: zone) }
                }
            } label: { Label("Play or queue this track", systemImage: "play.circle") }
                .buttonStyle(.bordered).disabled(!detail.canChoose || !remote.canPrepare)
                .accessibilityHint("Opens a review of this exact playlist occurrence and destination before sending.")
        }.frame(maxWidth: .infinity, alignment: .leading).remoteCard()
    }

    private func choose(_ item: PlaylistTrack, observed: PlaylistContentsObservation,
                        action: PlaylistTrackSelection.Action, in expected: Zone?) {
        do {
            guard remote.active, remote.canPrepare, remote.panel == .library,
                  let zone = expected, remote.selectedZone?.sameGroup(as: zone) == true else {
                throw ContractError.invalid("The destination changed or another action is active. Choose the track again.")
            }
            let selected = try detail.selection(item, from: observed.id, in: zone, action: action)
            actionProblem = nil
            remote.prepare(.playlistTrack(selected), in: zone)
        } catch { actionProblem = error.localizedDescription }
    }
    private func cacheTime(_ seconds: Int64) -> String {
        guard (-62_135_596_800...253_402_300_799).contains(seconds) else { return "Cache timestamp: Unix second \(seconds)" }
        return "Cached " + Date(timeIntervalSince1970: TimeInterval(seconds)).formatted(date: .abbreviated, time: .standard)
    }
}
#endif
