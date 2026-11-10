#if os(macOS) || os(iOS)
import SwiftUI
import FrankenSonosKit

/// Online search is a separate, explicit read. Result actions enter the same
/// command review as pasted sources; the search model has no writer.
@MainActor struct SpotifySearchPanel: View {
    @Environment(\.scenePhase) private var scenePhase
    @ObservedObject var bridge: RemoteBridge
    @State private var selectionProblem: String?
    private var search: SpotifySearchModel { bridge.spotifySearch }
    private var remote: RemoteModel { bridge.model }

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("Search Spotify using your daemon’s connected account. Choose a category and search explicitly; your saved library stays separate.")
                .font(.callout).foregroundStyle(.secondary)
            Picker("Spotify category", selection: Binding(
                get: { search.kind }, set: { selectionProblem = nil; search.chooseKind($0) })) {
                ForEach(SpotifyCatalogKind.allCases, id: \.self) { Text($0.title).tag($0) }
            }.pickerStyle(.menu).disabled(!search.active)
            TextField("Search Spotify", text: Binding(
                get: { search.query }, set: { selectionProblem = nil; search.edit($0) }))
                .textFieldStyle(.roundedBorder).accessibilityLabel("Search Spotify online")
                .disabled(!search.active)
                #if os(iOS)
                .submitLabel(.search)
                .autocorrectionDisabled()
                #endif
                .onSubmit { runSearch() }
            if !search.query.isEmpty && (try? SpotifySearchRequest(query: search.query, kind: search.kind)) == nil {
                Text("Use 1–256 UTF-8 bytes without control characters.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            HStack(spacing: 12) {
                Button("Search Spotify", systemImage: "magnifyingglass") { runSearch() }
                    .buttonStyle(.bordered).disabled(!search.canSearch)
                if search.loading {
                    ProgressView().controlSize(.small).accessibilityLabel("Searching Spotify")
                    Button("Cancel") { search.cancel() }.buttonStyle(.bordered)
                }
            }
            if let problem = search.problem ?? selectionProblem {
                Label(problem, systemImage: "exclamationmark.circle")
                    .font(.callout).textSelection(.enabled).fixedSize(horizontal: false, vertical: true)
            }
            if let observed = search.observation { results(observed) }
            else if !search.loading && search.problem == nil {
                EmptyMusicState(title: "Find your next listen", systemImage: "magnifyingglass",
                                detail: "Search tracks, albums, artists, playlists, shows or episodes. Searching works even before a room is selected.")
            }
        }
        .onAppear { resume() }
        .onDisappear { search.suspend(); selectionProblem = nil }
        .onChange(of: scenePhase) { _, phase in
            if phase == .background { search.suspend() }
            else if phase == .active { resume() }
        }
        .onChange(of: remote.active) { _, active in
            if active { resume() } else { search.suspend() }
        }
    }

    private func resume() {
        if scenePhase == .active && remote.active { search.resume() }
    }
    private func runSearch() { selectionProblem = nil; search.search() }

    private func results(_ observed: SpotifySearchObservation) -> some View {
        VStack(alignment: .leading, spacing: 14) {
            Text(search.isCurrent ? "Spotify results" : "Previous results · search again before choosing")
                .font(.headline)
            Text("\(observed.page.kind.title) for “\(observed.page.query)” · \(observed.page.items.count) results")
                .font(.subheadline).foregroundStyle(.secondary)
            Text("Read \(observed.receivedAt.formatted(date: .abbreviated, time: .shortened))")
                .font(.caption).foregroundStyle(.secondary)
            if observed.page.more {
                Text("More matches exist. Refine the query to narrow this first page.")
                    .font(.callout).foregroundStyle(.secondary)
            }
            if observed.page.items.isEmpty {
                Text("No matches in this category. Try another search or category.").font(.callout)
            }
            if let zone = remote.selectedZone {
                Text("Playback destination: \(zone.title) · \(zone.household)").font(.callout)
            } else {
                Label("Choose a room when you’re ready to play or queue a result.", systemImage: "hifispeaker")
                    .font(.callout).foregroundStyle(.secondary)
            }
            LazyVStack(alignment: .leading, spacing: 12) {
                ForEach(observed.page.items) { item in
                    resultRow(item, observed: observed, target: remote.selectedZone)
                }
            }
        }
    }

    private func resultRow(_ item: SpotifySearchItem, observed: SpotifySearchObservation, target: Zone?) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            Text(item.title).font(.headline).textSelection(.enabled)
            if let subtitle = item.subtitle, !subtitle.isEmpty {
                Text(subtitle).font(.subheadline).foregroundStyle(.secondary).textSelection(.enabled)
            }
            if observed.page.kind == .artist {
                Text("Plays up to 10 tracks from this artist’s top tracks or a bounded album sample.")
                    .font(.caption).foregroundStyle(.secondary)
            } else if observed.page.kind == .show {
                Text("Plays up to 10 available episodes from the show’s first page, in Spotify’s returned order.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            HStack(spacing: 12) {
                Button("Play now", systemImage: "play.fill") {
                    choose(item, observed: observed, target: target, action: .playNow)
                }.buttonStyle(.borderedProminent).foregroundStyle(RemoteStyle.accentInk)
                Menu {
                    Button("Queue next", systemImage: "text.line.first.and.arrowtriangle.forward") {
                        choose(item, observed: observed, target: target, action: .queueNext)
                    }
                    Button("Add to end of queue", systemImage: "text.append") {
                        choose(item, observed: observed, target: target, action: .queueLast)
                    }
                } label: { Label("Queue", systemImage: "text.badge.plus") }
                    .buttonStyle(.bordered)
            }
            .disabled(!search.canChoose || !remote.canPrepare || target == nil)
            .accessibilityHint("Opens the usual review for this source and destination. Nothing plays until you confirm.")
        }.remoteCard()
    }

    private func choose(_ item: SpotifySearchItem, observed: SpotifySearchObservation, target: Zone?,
                        action: SpotifySearchSelection.Action) {
        do {
            guard remote.canPrepare, let target,
                  remote.selectedZone?.sameGroup(as: target) == true else {
                throw ContractError.invalid("Choose the current destination group before reviewing this source.")
            }
            let selected = try search.selection(item, from: observed.id, in: target, action: action)
            selectionProblem = nil
            remote.prepare(selected.command, in: selected.zone)
        } catch { selectionProblem = error.localizedDescription }
    }
}
#endif
