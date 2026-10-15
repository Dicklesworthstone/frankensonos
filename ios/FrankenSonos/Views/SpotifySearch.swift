import SwiftUI

/// Search results from all of Spotify for the Search tab (live mode). Tapping an album or a song plays it in the
/// selected room; the speaker plays through the account linked in Sonos, so only the search itself needs the sign-in.
struct SpotifySearchResultsView: View {
    @EnvironmentObject private var store: MockZoneStore
    @Environment(\.colorScheme) private var scheme
    @StateObject private var model = SpotifyModel()
    let query: String
    var openPlayer: () -> Void
    @ScaledMetric private var rowHeight = 56.0

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            content
        }
        .task(id: store.daemonURLString) {
            model.attach(store.daemonClient)
            await model.load()
        }
        .task(id: query) {
            // A pause in typing, then one search; a newer keystroke cancels this one.
            try? await Task.sleep(for: .milliseconds(350))
            guard !Task.isCancelled else { return }
            await model.searchCatalog(query)
        }
    }

    @ViewBuilder private var content: some View {
        if let status = model.status, !status.configured || !status.signedIn {
            note("Sign in to Spotify to search it", "Open the Music tab, choose Spotify, and sign in once.")
        } else if let why = model.catalogError {
            note("Search failed", why)
        } else if query.trimmingCharacters(in: .whitespaces).isEmpty {
            note("Search all of Spotify", "Type an artist, song or album.")
        } else if let found = model.catalog {
            if found.isEmpty {
                note("Nothing found", "No albums or songs match \"\(query)\".")
            } else {
                results(found)
            }
        } else {
            ProgressView().frame(maxWidth: .infinity).padding(32)
        }
    }

    @ViewBuilder private func results(_ found: SpotifySearchResults) -> some View {
        if !found.albums.isEmpty {
            heading("Albums")
            ForEach(found.albums) { item in
                row(art: item.artUrl, title: item.title,
                    subtitle: [item.artist, item.year.map(String.init)].compactMap { $0 }.joined(separator: " · ")) {
                    play(item.uri, item.title)
                }
            }
        }
        if !found.tracks.isEmpty {
            heading("Songs")
            ForEach(found.tracks) { item in
                row(art: item.artUrl, title: item.title,
                    subtitle: [item.artistLine, item.album].compactMap { $0 }.filter { !$0.isEmpty }.joined(separator: " · ")) {
                    play(item.uri, item.title)
                }
            }
        }
    }

    private func play(_ uri: String, _ title: String) {
        store.playSpotify(uri: uri, title: title)
        openPlayer()
    }

    private func heading(_ text: String) -> some View {
        Text(text).s1Font(17, weight: .semibold).padding(.horizontal, 16).padding(.vertical, 12)
    }

    private func note(_ title: String, _ message: String) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(title).s1Font(17, weight: .semibold)
            Text(message).s1Font(13).foregroundStyle(S1Palette.secondary(scheme)).fixedSize(horizontal: false, vertical: true)
        }
        .padding(16)
    }

    private func row(art: URL?, title: String, subtitle: String, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            HStack(spacing: 12) {
                SpotifyArt(url: art, size: 40)
                VStack(alignment: .leading, spacing: 3) {
                    Text(title).s1Font(14, weight: .medium).lineLimit(1)
                    if !subtitle.isEmpty { Text(subtitle).s1Font(12).foregroundStyle(S1Palette.secondary(scheme)).lineLimit(1) }
                }
                Spacer(minLength: 0)
            }
            .padding(.horizontal, 16).frame(minHeight: rowHeight)
            .overlay(alignment: .bottom) { Rectangle().fill(Color.primary.opacity(0.08)).frame(height: 0.5).padding(.leading, 68) }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .disabled(store.isOffline(store.selectedZone))
    }
}
