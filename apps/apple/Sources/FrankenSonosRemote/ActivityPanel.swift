#if os(macOS) || os(iOS)
import SwiftUI
import FrankenSonosKit

@MainActor struct ActivityButton: View {
    @ObservedObject var bridge: RemoteBridge
    @Binding var isPresented: Bool
    var body: some View {
        Button("Activity & History", systemImage: "clock.arrow.circlepath") { isPresented = true }
            .disabled(!bridge.model.canSchedule)
            .help("Read the daemon's action log and recorded plays. No speaker selection is required.")
    }
}

@MainActor struct ActivityPanel: View {
    @Environment(\.dismiss) private var dismiss
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @ObservedObject var bridge: RemoteBridge
    let replay: (ActivityReplaySelection) -> Void
    @State private var tab = 0
    @State private var search = ""
    @State private var clientFilter = ""
    @State private var historyRoom: RoomID?
    @State private var limit = 50
    @State private var useStart = false
    @State private var start = Date().addingTimeInterval(-86_400)
    @State private var selectedPlay: UUID?
    @State private var target: Zone?
    @State private var mode: ActivityModel.Replay = .play
    @State private var selectionProblem: String?
    private var model: ActivityModel { bridge.activity }
    private var zones: [Zone] { bridge.model.house?.snapshot?.zones ?? [] }
    private var selected: ActivityModel.Entry? { model.history.first { $0.id == selectedPlay } }
    private var actions: [LoggedAction] {
        model.actions.filter { matches([$0.client, $0.surface, $0.intent, $0.decision, $0.result, String($0.id)]) }
    }
    private var history: [ActivityModel.Entry] {
        model.history.filter { matches([$0.play.title, $0.play.artist, $0.play.room, $0.play.sourceUri].compactMap { $0 }) }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack {
                Text("Activity & History").font(.title2.bold())
                Spacer()
                if model.loading { ProgressView().controlSize(.small) }
                Button { selectedPlay = nil; model.refresh() } label: { Image(systemName: "arrow.clockwise").frame(width: 44, height: 44) }
                    .buttonStyle(.plain).disabled(!model.active || model.loading).accessibilityLabel("Refresh activity and history")
                Button { dismiss() } label: { Image(systemName: "xmark").frame(width: 44, height: 44) }
                    .buttonStyle(.plain).keyboardShortcut(.cancelAction).accessibilityLabel("Close activity and history")
            }
            ScrollViewReader { proxy in
                ScrollView {
                    VStack(alignment: .leading, spacing: 14) {
                        Text("\(bridge.endpoint) • Latest records, not a complete archive. Times are displayed in this device’s time zone.")
                            .font(.caption).foregroundStyle(.secondary).textSelection(.enabled)
                        filters
                        if let problem = model.filterProblem { Text(problem).foregroundStyle(.red) }
                        Picker("Records", selection: $tab) {
                            Text("Action log").tag(0)
                            Text("Listening history").tag(1)
                        }.pickerStyle(.segmented)
                        TextField("Search the loaded records (does not make a request)", text: $search).textFieldStyle(.roundedBorder)
                            .onChange(of: search) { _, _ in selectedPlay = nil }
                        if tab == 0 {
                            freshness(model.actionsObservedAt, stale: model.actionsStale, problem: model.actionsProblem)
                            Text("Active filter: \(model.query.client ?? "all clients"); \(model.query.since.map { "from unix second " + String($0) } ?? "all recorded times").")
                                .font(.caption)
                            LazyVStack(alignment: .leading, spacing: 12) {
                                ForEach(actions) { row in actionRow(row) }
                                if actions.isEmpty { empty(model.actionsObservedAt, problem: model.actionsProblem) }
                            }
                        } else {
                            freshness(model.historyObservedAt, stale: model.historyStale, problem: model.historyProblem)
                            Text("Active filter: \(model.query.historyRoom?.address ?? "all recorded groups"). Room labels may be absent or ambiguous across households. A recorded play is not proof of a full listen.")
                                .font(.caption)
                            LazyVStack(alignment: .leading, spacing: 12) {
                                ForEach(history) { entry in historyRow(entry) }
                                if history.isEmpty { empty(model.historyObservedAt, problem: model.historyProblem) }
                            }
                            replayControls.id("activity-replay")
                        }
                    }.frame(maxWidth: .infinity, alignment: .leading)
                }
                .onChange(of: selectedPlay) { _, selected in
                    if selected != nil {
                        withAnimation(reduceMotion ? nil : .easeInOut(duration: 0.2)) {
                            proxy.scrollTo("activity-replay", anchor: .bottom)
                        }
                    }
                }
            }
        }
        .padding(20).frame(maxWidth: 1_100)
        .background(RemoteStyle.background)
        #if os(macOS)
        .frame(minWidth: 680, idealWidth: 780, minHeight: 550, idealHeight: 740)
        #endif
        .presentationDetents([.large])
        .presentationDragIndicator(.visible)
        .onAppear {
            clientFilter = model.query.client ?? ""; historyRoom = model.query.historyRoom; limit = model.query.limit
            useStart = model.query.since != nil
            if let since = model.query.since { start = Date(timeIntervalSince1970: Double(since)) }
            if bridge.model.active { model.resume() }
        }
        .onDisappear { model.suspend() }
        .onChange(of: zones) { _, current in
            if let chosen = target, !current.contains(where: { $0.sameGroup(as: chosen) }) {
                target = nil; selectionProblem = "The destination group changed. Choose it again."
            }
        }
    }

    private var filters: some View {
        DisclosureGroup("Read filters (apply explicitly)") {
            VStack(alignment: .leading, spacing: 10) {
                TextField("Exact action-log client; blank means all", text: $clientFilter).textFieldStyle(.roundedBorder)
                VStack(alignment: .leading, spacing: 8) {
                    Toggle("Action log from", isOn: $useStart)
                    DatePicker("This device’s local time", selection: $start, displayedComponents: [.date, .hourAndMinute])
                        .disabled(!useStart)
                }
                Picker("Listening-history group", selection: $historyRoom) {
                    Text("All recorded groups").tag(Optional<RoomID>.none)
                    ForEach(zones) { zone in Text("\(zone.title) [\(zone.household)]").tag(Optional(zone.id)) }
                    if let historyRoom, !zones.contains(where: { $0.id == historyRoom }) {
                        Text("\(historyRoom.address) (not currently listed)").tag(Optional(historyRoom))
                    }
                }
                VStack(alignment: .leading, spacing: 8) {
                    Picker("Maximum rows per feed", selection: $limit) {
                        ForEach([25, 50, 100, 200], id: \.self) { Text(String($0)).tag($0) }
                    }
                    Button("Apply filters") {
                        let since = useStart ? Int64(exactly: start.timeIntervalSince1970.rounded(.down)) : nil
                        guard !useStart || since != nil else { selectionProblem = "Choose a valid start time."; return }
                        let client = clientFilter.trimmingCharacters(in: .whitespacesAndNewlines)
                        model.apply(ActivityQuery(client: client.isEmpty ? nil : client, since: since, historyRoom: historyRoom, limit: limit))
                        selectedPlay = nil; selectionProblem = nil
                    }.disabled(!model.active)
                }
            }.padding(.top, 8)
        }
    }

    private func actionRow(_ row: LoggedAction) -> some View {
        GroupBox {
            VStack(alignment: .leading, spacing: 5) {
                Text("#\(String(row.id)) • \(row.date.formatted(date: .abbreviated, time: .standard))").font(.caption).monospacedDigit()
                Text(row.intent).font(.headline)
                Text("\(row.client) via \(row.surface)").font(.callout)
                Text("Decision: \(row.decision)")
                Text(row.result)
                if let undone = row.undoOf { Text("Undo of action #\(String(undone))").font(.caption) }
                if row.undoable { Text("Saved before-state exists; this is not a guarantee of current undo eligibility.").font(.caption).foregroundStyle(.secondary) }
            }.frame(maxWidth: .infinity, alignment: .leading).textSelection(.enabled)
        }
    }

    private func historyRow(_ entry: ActivityModel.Entry) -> some View {
        GroupBox {
            VStack(alignment: .leading, spacing: 5) {
                HStack {
                    Text(entry.play.displayTitle).font(.headline)
                    Spacer()
                    Button(selectedPlay == entry.id ? "Selected" : "Select source") {
                        selectedPlay = entry.id; selectionProblem = nil
                    }.disabled(!model.active || model.loading || model.historyStale || entry.play.replaySource == nil)
                }
                if let artist = entry.play.artist { Text(artist) }
                Text("\(entry.play.date.formatted(date: .abbreviated, time: .standard)) • Reported room: \(entry.play.room ?? "unavailable")")
                    .font(.caption).monospacedDigit()
                Text(entry.play.sourceUri).font(.caption.monospaced()).textSelection(.enabled)
                if entry.play.replaySource == nil {
                    Text("This renderer-specific or unsupported source is inspectable but cannot be replayed here.").font(.caption).foregroundStyle(.secondary)
                }
            }.frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    private var replayControls: some View {
        GroupBox("Replay a selected source") {
            VStack(alignment: .leading, spacing: 8) {
                if let selected { Text(selected.play.displayTitle).font(.headline) }
                else { Text("Select a supported source above, then choose its destination.") }
                Picker("Current destination", selection: Binding<RoomID?>(get: { target?.id }, set: { id in
                    target = zones.first { $0.id == id }; selectionProblem = nil
                })) {
                    Text("Choose a group and household").tag(Optional<RoomID>.none)
                    ForEach(zones) { zone in Text("\(zone.title) [\(zone.household)]").tag(Optional(zone.id)) }
                }
                VStack(alignment: .leading, spacing: 12) {
                    Picker("Action", selection: $mode) {
                        Text("Play now").tag(ActivityModel.Replay.play)
                        Text("Queue next").tag(ActivityModel.Replay.queueNext)
                        Text("Append to queue").tag(ActivityModel.Replay.queueLast)
                    }
                    Button("Review in remote") {
                        do {
                            guard let selectedPlay else { return }
                            let selection = try model.selectForReplay(selectedPlay, as: mode, target: target, current: zones)
                            replay(selection)
                        } catch { selectionProblem = error.localizedDescription }
                    }.buttonStyle(.borderedProminent).foregroundStyle(RemoteStyle.accentInk)
                        .disabled(selected == nil || target == nil || model.loading || model.historyStale || !model.active || !bridge.model.canSchedule)
                }
                Text("Nothing plays yet. This closes Activity, selects the chosen current group, and opens its ordinary command review. Play replaces its source; queue actions do not start music. Historical labels never select the destination.")
                    .font(.caption).foregroundStyle(.secondary)
                if let selectionProblem { Text(selectionProblem).foregroundStyle(.red) }
            }.frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    private func freshness(_ at: Date?, stale: Bool, problem: String?) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(at.map { "Last read: " + $0.formatted(date: .abbreviated, time: .standard) } ?? "No successful read yet")
                .font(.caption).foregroundStyle(.secondary)
            if !model.active || model.loading || stale { Text("Cached observation — refresh before using it.").font(.callout) }
            if let problem { Text(problem).textSelection(.enabled) }
        }
    }
    private func empty(_ observed: Date?, problem: String?) -> some View {
        Text(problem != nil ? "The read failed; an empty list here does not mean there were no records." :
                observed == nil ? "Reading records…" : search.isEmpty ? "No records returned for this filter." : "No loaded records match this search.")
            .foregroundStyle(.secondary)
    }
    private func matches(_ fields: [String]) -> Bool { search.isEmpty || fields.contains { $0.localizedStandardContains(search) } }
}
#endif
