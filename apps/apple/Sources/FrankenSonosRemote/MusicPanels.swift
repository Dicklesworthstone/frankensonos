#if os(macOS)
import SwiftUI
import FrankenSonosKit

@MainActor struct MusicPanels: View {
    @ObservedObject var bridge: RemoteBridge
    @State private var search = ""
    @State private var source = ""
    @State private var mood = ""
    @State private var minutes = 60
    private var model: RemoteModel { bridge.model }

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack {
                Text(model.panel.rawValue.capitalized).font(.title2.bold())
                Spacer()
                if model.panelLoading { ProgressView().controlSize(.small) }
                Button("Refresh panel") { model.refreshPanel() }.disabled(!model.active || model.panelLoading)
            }
            if let problem = model.panelProblem { Text(problem).textSelection(.enabled) }
            switch model.panel {
            case .queue: queuePanel
            case .favorites: favoritesPanel
            case .dj: djPanel
            case .library: libraryPanel
            case .scenes: ScenePanel(bridge: bridge)
            case .sleep: SleepPanel(bridge: bridge)
            }
        }
    }

    @ViewBuilder private var queuePanel: some View {
        let zone = model.selectedZone
        if let page = model.queue {
            Text("\(page.total) entries. Each page is a new observation; edits check the exact revision and entry again.")
                .font(.caption).foregroundStyle(.secondary)
            ForEach(page.items) { item in
                HStack(alignment: .top) {
                    Text("\(item.position)").monospacedDigit().frame(width: 38, alignment: .trailing)
                    VStack(alignment: .leading) {
                        Text(item.title).font(.headline)
                        Text([item.creator, item.album].compactMap { $0 }.joined(separator: " • ")).foregroundStyle(.secondary)
                    }
                    Spacer()
                    Button("Play") { if let revision = page.revision { model.prepare(.queuePlay(item, revision: revision), in: zone) } }
                    Button("Remove") { if let revision = page.revision { model.prepare(.queueRemove(item, revision: revision), in: zone) } }
                }
                .disabled(!model.canPrepare || model.panelLoading || !item.playable || page.revision?.isEmpty != false)
                Divider()
            }
            if page.items.isEmpty { Text("This queue page is empty.").foregroundStyle(.secondary) }
            if page.revision == nil { Text("The daemon supplied no queue revision. This page is read-only.") }
            HStack {
                Button("Previous page") { model.queuePage(offset: model.queueOffset >= 25 ? model.queueOffset - 25 : 0) }
                    .disabled(model.queueOffset == 0 || model.panelLoading)
                Spacer()
                Button("Next page") { if let next = UInt32(exactly: UInt64(model.queueOffset) + 25) { model.queuePage(offset: next) } }
                    .disabled(UInt64(model.queueOffset) + 25 >= UInt64(page.total) || model.panelLoading)
            }
        }
        GroupBox("Play or queue a source") {
            VStack(alignment: .leading) {
                TextField("Spotify URI or another supported source URI", text: $source).textFieldStyle(.roundedBorder)
                sourceButtons(source)
            }.padding(4)
        }
    }

    @ViewBuilder private var favoritesPanel: some View {
        let zone = model.selectedZone
        VStack(alignment: .leading, spacing: 12) {
            if !model.panelLoading && model.favorites.isEmpty { Text("No favorites were returned for this household.") }
            ForEach(model.favorites) { favorite in
                HStack {
                    VStack(alignment: .leading) {
                        Text(favorite.title).font(.headline)
                        if let description = favorite.description { Text(description).font(.callout).foregroundStyle(.secondary) }
                    }
                    Spacer()
                    Button("Review playback") { model.prepare(.playFavorite(favorite.id), in: zone) }
                        .disabled(!model.canPrepare || model.panelLoading || !favorite.playable)
                }
                Divider()
            }
        }
    }

    @ViewBuilder private var djPanel: some View {
        let zone = model.selectedZone
        if let status = model.dj {
            Text(status.running ? "DJ running" : "DJ stopped").font(.headline)
            if let problem = status.problem { Text(problem).textSelection(.enabled) }
            if let work = status.now {
                Text("\(work.composer) — \(work.title)").font(.title3)
                if let movement = work.movement { Text("Movement \(movement) of \(work.movements)") }
                Text(work.reason.summary).foregroundStyle(.secondary)
            }
            HStack {
                Button("Skip whole work") { model.prepare(.djSkip, in: zone) }
                Button("Like") { model.prepare(.djFeedback(like: true), in: zone) }
                Button("Dislike") { model.prepare(.djFeedback(like: false), in: zone) }
                Button("Stop DJ") { model.prepare(.djStop, in: zone) }
            }.disabled(!model.canPrepare || !status.running || model.panelLoading)
            if !status.next.isEmpty {
                Text("Coming next").font(.headline)
                ForEach(Array(status.next.enumerated()), id: \.offset) { _, work in
                    Text("\(work.composer) — \(work.title) • \(work.movements) movement(s)")
                }
            }
            Divider()
            Picker("Mood", selection: $mood) {
                Text("Time-of-day program").tag("")
                ForEach(model.moods) { Text($0.name).tag($0.name) }
            }
            HStack {
                Button("Start DJ") { model.prepare(.djStart(mood: mood.isEmpty ? nil : mood), in: zone) }
                Picker("For", selection: $minutes) {
                    ForEach([15, 30, 60, 120, 240], id: \.self) { Text("\($0) minutes").tag($0) }
                }.frame(width: 190)
                Button("Steer future picks") { model.prepare(.djSteer(mood: mood, seconds: UInt32(minutes * 60)), in: zone) }
                    .disabled(mood.isEmpty)
                Button("Clear steering") { model.prepare(.djClear, in: zone) }
            }.disabled(!model.canPrepare || model.panelLoading || (!mood.isEmpty && !model.moods.contains { $0.name == mood }))
            Text("Steering: \(status.steering.mood ?? status.steering.source). Changes affect future picks; queued works stay whole.")
                .font(.caption).foregroundStyle(.secondary)
        } else if !model.panelLoading && model.panelProblem == nil {
            Text("Refresh to read the DJ session.")
        }
    }

    @ViewBuilder private var libraryPanel: some View {
        let zone = model.selectedZone
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                TextField("Search your cached music library", text: $search).textFieldStyle(.roundedBorder)
                    .onSubmit { model.search(search) }
                Button("Search") { model.search(search) }.disabled(!model.active)
            }
            Text("Results belong to the selected household. Play replaces its current source; Queue adds without starting playback.")
                .font(.caption).foregroundStyle(.secondary)
            ForEach(Array(model.hits.enumerated()), id: \.offset) { _, hit in
                VStack(alignment: .leading, spacing: 6) {
                    Text(hit.title).font(.headline)
                    if let subtitle = hit.subtitle { Text(subtitle).foregroundStyle(.secondary) }
                    if let uri = hit.sourceUri { sourceButtons(uri) }
                    else if let favorite = hit.favorite {
                        Button("Review favorite playback") { model.prepare(.playFavorite(favorite), in: zone) }
                            .disabled(!model.canPrepare || model.panelLoading)
                    }
                }
                Divider()
            }
            if !model.query.isEmpty && !model.panelLoading && model.hits.isEmpty && model.panelProblem == nil {
                Text("No matches for “\(model.query)”.")
            }
        }
    }

    @ViewBuilder private func sourceButtons(_ uri: String) -> some View {
        let zone = model.selectedZone
        HStack {
            Button("Review play") { model.prepare(.playSource(uri), in: zone) }
            Button("Queue next") { model.prepare(.queueAdd(uri, next: true), in: zone) }
            Button("Append to queue") { model.prepare(.queueAdd(uri, next: false), in: zone) }
        }.disabled(!model.canPrepare || model.panelLoading || uri.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
    }
}
#endif
