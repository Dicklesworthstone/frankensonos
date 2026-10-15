import SwiftUI

/// Spotify inside Browse (live mode): the daemon's sign-in state, then the owner's albums and liked tracks, search,
/// an album's tracks, and the classical DJ. Tapping something plays it in the selected room.
struct SpotifyBrowse: View {
    @EnvironmentObject private var store: MockZoneStore
    @Environment(\.colorScheme) private var scheme
    @StateObject private var model = SpotifyModel()
    @State private var section = "Albums"
    @State private var query = ""
    @State private var album: SpotifyAlbum?
    @State private var albumTracks: [SpotifyTrack] = []
    @State private var signInSheet = SpotifySignInSheet()
    @State private var linkNote: String?
    var openPlayer: () -> Void
    @ScaledMetric private var rowHeight = 56.0

    init(model: SpotifyModel? = nil, openPlayer: @escaping () -> Void = {}) {
        _model = StateObject(wrappedValue: model ?? SpotifyModel())
        self.openPlayer = openPlayer
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            linkRow
            switch model.phase {
            case .loading:
                ProgressView().frame(maxWidth: .infinity).padding(32)
            case .unreachable(let why):
                notice("Can't read Spotify from the daemon", why, action: "Try again") { await model.load() }
            case .notConfigured:
                notice("Spotify isn't set up on the daemon",
                       "Give the daemon your Spotify app's Client ID (FSONOS_SPOTIFY_CLIENT_ID) and add frankensonos://spotify-callback to that app's redirect URIs, then restart it.",
                       action: "Check again") { await model.load() }
            case .signedOut(let reauthorize):
                notice(reauthorize ? "Spotify needs you to sign in again" : "Sign in to Spotify",
                       "Spotify asks once for permission to read your saved albums and liked tracks. The daemon keeps the sign-in, so every room and every phone can play from your library.",
                       action: "Sign in with Spotify") { await signIn() }
                if let error = model.error { Text(error).s1Font(12).foregroundStyle(.red).padding(.horizontal, 16) }
            case .empty:
                emptyLibrary
            case .ready:
                if let album { albumDetail(album) } else { library }
            }
        }
        .task(id: store.daemonURLString) {
            guard !model.isSample else { return }
            model.attach(store.daemonClient)
            await model.load()
        }
    }

    // MARK: pasted link

    /// Plays a Spotify link from the clipboard. It needs no sign-in: the speaker plays through the account linked in Sonos.
    private var linkRow: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                PasteButton(payloadType: String.self) { strings in
                    Task { @MainActor in play(link: strings.first ?? "") }
                }
                .buttonBorderShape(.capsule).labelStyle(.titleAndIcon)
                Text("Copy a link in Spotify, then paste it here.").s1Font(12).foregroundStyle(S1Palette.secondary(scheme))
            }
            if let linkNote { Text(linkNote).s1Font(12).foregroundStyle(linkNote.hasPrefix("Playing") ? S1Palette.secondary(scheme) : .red) }
        }
        .padding(.horizontal, 16).padding(.top, 12).padding(.bottom, 4)
    }

    private func play(link text: String) {
        switch SpotifyLink.parse(text) {
        case .failure(let error):
            linkNote = error.message
        case .success(let link):
            guard store.zones.contains(where: { $0.id == store.selectedZoneID }) else { linkNote = "Pick a room first."; return }
            store.playSpotify(uri: link.uri, title: "Spotify \(link.kind)")
            linkNote = "Playing the \(link.kind) in \(store.selectedZone.displayName)."
        }
    }

    // MARK: states

    private func signIn() async {
        await model.signIn { try await signInSheet($0) }
    }

    private func notice(_ title: String, _ message: String, action: String, perform: @escaping () async -> Void) -> some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(title).s1Font(17, weight: .semibold)
            Text(message).s1Font(13).foregroundStyle(S1Palette.secondary(scheme)).fixedSize(horizontal: false, vertical: true)
            Button(action) { Task { await perform() } }.s1Font(14, weight: .semibold).buttonStyle(.plain).frame(minHeight: 44)
        }
        .padding(16)
    }

    @ViewBuilder private var emptyLibrary: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Your library hasn't been read yet").s1Font(17, weight: .semibold)
            if let sync = model.status?.sync, sync.running {
                ProgressView(value: Double(sync.done), total: Double(max(sync.total, 1)))
                Text("Reading your Spotify library: \(sync.done) of \(sync.total)").s1Font(12).foregroundStyle(S1Palette.secondary(scheme))
            } else {
                Text("The daemon reads your saved albums and liked tracks once, then keeps them for browsing and the DJ.")
                    .s1Font(13).foregroundStyle(S1Palette.secondary(scheme)).fixedSize(horizontal: false, vertical: true)
                Button("Sync library") { Task { await model.sync() } }.s1Font(14, weight: .semibold).buttonStyle(.plain).frame(minHeight: 44)
            }
            if let error = model.error { Text(error).s1Font(12).foregroundStyle(.red) }
        }
        .padding(16)
    }

    // MARK: library

    @ViewBuilder private var library: some View {
        djControls
        syncLine
        if let error = model.error { Text(error).s1Font(12).foregroundStyle(.red).padding(.horizontal, 16).padding(.bottom, 8) }
        HStack(spacing: 8) {
            Image(systemName: "magnifyingglass").foregroundStyle(S1Palette.secondary(scheme))
            TextField("Search your library", text: $query).textFieldStyle(.plain).s1Font(14).accessibilityLabel("Search your Spotify library")
            if !query.isEmpty {
                Button { query = "" } label: { Image(systemName: "xmark.circle.fill").foregroundStyle(S1Palette.secondary(scheme)) }
                    .buttonStyle(.plain).accessibilityLabel("Clear search")
            }
        }
        .padding(.horizontal, 12).frame(height: 40)
        .background(S1Palette.field(scheme), in: RoundedRectangle(cornerRadius: 7))
        .padding(.horizontal, 16).padding(.bottom, 12)
        .task(id: query) {
            // A pause in typing, then one search; a newer keystroke cancels this one.
            if query != model.query { try? await Task.sleep(for: .milliseconds(300)) }
            guard !Task.isCancelled else { return }
            await model.search(query)
        }
        Picker("Library", selection: $section) {
            Text("Albums (\(model.albumsTotal))").tag("Albums")
            Text("Liked (\(model.likedTotal))").tag("Liked")
        }
        .pickerStyle(.segmented).padding(.horizontal, 16).padding(.bottom, 8)
        if section == "Albums" {
            LazyVStack(spacing: 0) {
                ForEach(model.albums) { item in
                    row(art: item.artUrl, seed: item.title, title: item.title,
                        subtitle: [item.artist, item.year.map(String.init)].compactMap { $0 }.joined(separator: " · "), chevron: true) {
                        album = item
                        albumTracks = []
                        Task { albumTracks = await model.tracks(for: item) }
                    }
                    .onAppear { if item.id == model.albums.last?.id { Task { await model.loadMoreAlbums() } } }
                }
            }
            if model.albums.isEmpty { empty("No albums match") }
        } else {
            LazyVStack(spacing: 0) {
                ForEach(model.liked) { item in
                    row(art: item.artUrl, seed: item.title, title: item.title,
                        subtitle: [item.artistLine, item.album].compactMap { $0 }.filter { !$0.isEmpty }.joined(separator: " · "), chevron: false) {
                        play(item.uri, item.title)
                    }
                    .onAppear { if item.id == model.liked.last?.id { Task { await model.loadMoreLiked() } } }
                }
            }
            if model.liked.isEmpty { empty("No liked tracks match") }
        }
    }

    private var djControls: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("Classical DJ in \(store.selectedZone.displayName)").s1Font(11).foregroundStyle(S1Palette.secondary(scheme))
            HStack(spacing: 8) {
                djButton("Start", "play.fill") { store.dj("start") }
                djButton("Skip", "forward.fill") { store.dj("skip") }
                djButton("Stop", "stop.fill") { store.dj("stop") }
            }
        }
        .padding(16)
        .disabled(store.isOffline(store.selectedZone))
    }

    private func djButton(_ title: String, _ symbol: String, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Label(title, systemImage: symbol).s1Font(13, weight: .semibold).frame(maxWidth: .infinity, minHeight: 40)
                .background(S1Palette.field(scheme), in: RoundedRectangle(cornerRadius: 8))
        }
        .buttonStyle(.plain)
    }

    @ViewBuilder private var syncLine: some View {
        HStack {
            if let sync = model.status?.sync, sync.running {
                ProgressView().controlSize(.small)
                Text("Reading your library: \(sync.done) of \(sync.total)").s1Font(12)
            } else {
                Text(summary).s1Font(12).foregroundStyle(S1Palette.secondary(scheme))
            }
            Spacer()
            Button("Switch account") { Task { await signIn() } }.s1Font(13).buttonStyle(.plain)
                .foregroundStyle(S1Palette.secondary(scheme)).padding(.trailing, 8)
            Button("Sync") { Task { await model.sync() } }.s1Font(13, weight: .semibold).buttonStyle(.plain)
                .disabled(model.status?.sync.running == true)
        }
        .padding(.horizontal, 16).padding(.bottom, 12)
    }

    private var summary: String {
        guard let library = model.status?.library else { return "" }
        let when = library.syncedAt.map { Date(timeIntervalSince1970: TimeInterval($0)).formatted(.relative(presentation: .named)) }
        return "\(library.albums) albums · \(library.tracks) liked tracks" + (when.map { " · synced \($0)" } ?? "")
    }

    // MARK: album

    @ViewBuilder private func albumDetail(_ album: SpotifyAlbum) -> some View {
        Button { self.album = nil } label: { Label("Library", systemImage: "chevron.left").s1Font(13, weight: .medium) }
            .buttonStyle(.plain).padding(.horizontal, 16).frame(height: 44)
        HStack(alignment: .top, spacing: 16) {
            SpotifyArt(url: album.artUrl, size: 96)
            VStack(alignment: .leading, spacing: 6) {
                Text(album.title).s1Font(18, weight: .semibold).fixedSize(horizontal: false, vertical: true)
                Text([album.artist, album.year.map(String.init)].compactMap { $0 }.joined(separator: " · "))
                    .s1Font(13).foregroundStyle(S1Palette.secondary(scheme))
                Button { play(album.uri, album.title) } label: {
                    Label("Play album", systemImage: "play.fill").s1Font(13, weight: .semibold).padding(.horizontal, 14).frame(height: 36)
                        .background(S1Palette.chipSelected(scheme), in: Capsule()).foregroundStyle(S1Palette.chipSelectedText(scheme))
                }
                .buttonStyle(.plain).padding(.top, 4)
            }
        }
        .padding(16)
        if albumTracks.isEmpty { ProgressView().frame(maxWidth: .infinity).padding(24) }
        ForEach(albumTracks) { track in
            row(art: nil, seed: track.title, title: track.title, subtitle: track.artistLine, trailing: track.durationText, chevron: false, showsArt: false) {
                play(track.uri, track.title)
            }
        }
    }

    // MARK: pieces

    private func play(_ uri: String, _ title: String) {
        store.playSpotify(uri: uri, title: title)
        openPlayer()
    }

    private func empty(_ text: String) -> some View {
        Text(text).s1Font(13).foregroundStyle(S1Palette.secondary(scheme)).padding(16)
    }

    private func row(art: URL?, seed: String, title: String, subtitle: String, trailing: String = "", chevron: Bool,
                     showsArt: Bool = true, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            HStack(spacing: 12) {
                if showsArt { SpotifyArt(url: art, size: 40) }
                VStack(alignment: .leading, spacing: 3) {
                    Text(title).s1Font(14, weight: .medium).lineLimit(1)
                    if !subtitle.isEmpty { Text(subtitle).s1Font(12).foregroundStyle(S1Palette.secondary(scheme)).lineLimit(1) }
                }
                Spacer(minLength: 0)
                if !trailing.isEmpty { Text(trailing).s1Font(12).monospacedDigit().foregroundStyle(S1Palette.secondary(scheme)) }
                if chevron { Image(systemName: "chevron.right").font(.system(size: 11)).foregroundStyle(S1Palette.secondary(scheme)) }
            }
            .padding(.horizontal, 16).frame(minHeight: rowHeight)
            .overlay(alignment: .bottom) { Rectangle().fill(Color.primary.opacity(0.08)).frame(height: 0.5).padding(.leading, showsArt ? 68 : 16) }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .disabled(store.isOffline(store.selectedZone))
    }
}

/// An album cover from the URL Spotify gave the daemon, or a plain tile when there is none or it cannot be loaded.
struct SpotifyArt: View {
    let url: URL?
    var size: CGFloat = 40
    @Environment(\.colorScheme) private var scheme

    var body: some View {
        Group {
            if let url {
                AsyncImage(url: url) { phase in
                    if let image = phase.image { image.resizable().scaledToFill() } else { placeholder }
                }
            } else {
                placeholder
            }
        }
        .frame(width: size, height: size)
        .clipShape(RoundedRectangle(cornerRadius: size > 60 ? 8 : 4, style: .continuous))
    }

    private var placeholder: some View {
        ZStack {
            S1Palette.field(scheme)
            Image(systemName: "music.note").font(.system(size: size * 0.4, weight: .light)).foregroundStyle(S1Palette.secondary(scheme))
        }
    }
}
