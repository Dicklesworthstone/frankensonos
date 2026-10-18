#if os(macOS)
import SwiftUI
import FrankenSonosKit

@MainActor struct RemoteScreen: View {
    @ObservedObject var bridge: RemoteBridge
    let disconnect: () -> Void
    @State private var seek = ""
    private var model: RemoteModel { bridge.model }

    var body: some View {
        NavigationSplitView {
            List(selection: Binding<RoomID?>(get: { model.selectedRoom }, set: { if let room = $0 { model.select(room) } })) {
                ForEach(households, id: \.self) { household in
                    Section(household) {
                        ForEach(model.house?.snapshot?.zones.filter { $0.household == household } ?? []) { zone in
                            ForEach(zone.members, id: \.self) { room in
                                VStack(alignment: .leading) {
                                    Text(room)
                                    Text(zone.members.count > 1 ? "Group: \(zone.coordinatorRoom)" : "Plays independently")
                                        .font(.caption).foregroundStyle(.secondary)
                                }
                                .tag(RoomID(household: household, room: room))
                            }
                        }
                    }
                }
            }
            .navigationTitle("Your rooms")
            .navigationSplitViewColumnWidth(min: 200, ideal: 240)
            .overlay { if households.isEmpty { Text("Reading the house…").foregroundStyle(.secondary) } }
        } detail: {
            ScrollView {
                VStack(alignment: .leading, spacing: 20) {
                    status
                    if let result = model.result {
                        GroupBox {
                            VStack(alignment: .leading, spacing: 8) {
                                HStack {
                                    Text(result.title).font(.headline)
                                    Spacer()
                                    Button("Dismiss") { model.dismissResult() }
                                }
                                Text("\(result.kind.rawValue.capitalized) • \(result.target.address)").font(.caption)
                                Text(result.detail).textSelection(.enabled)
                            }.frame(maxWidth: .infinity, alignment: .leading)
                        }
                        .accessibilityElement(children: .contain)
                    }
                    if model.actionPhase != .idle {
                        HStack {
                            ProgressView().controlSize(.small)
                            Text(model.actionPhase == .preparing ? "Reading before review…" : "Checking and sending once…")
                            Spacer()
                            Button("Cancel request") { model.cancelReview() }
                                .help("Stops unsent work. A sent action may already have taken effect; this is not Undo.")
                        }
                    }
                    if let zone = model.selectedZone {
                        Text(zone.title).font(.largeTitle.bold())
                        Text("Household: \(zone.household)").font(.caption).foregroundStyle(.secondary)
                        playback(zone)
                        levels(zone)
                        layout(zone)
                        Picker("Browse", selection: Binding(get: { model.panel }, set: { model.show($0) })) {
                            ForEach(RemoteModel.Panel.allCases, id: \.self) { Text($0.rawValue.capitalized).tag($0) }
                        }.pickerStyle(.segmented)
                        MusicPanels(bridge: bridge)
                    } else {
                        ContentUnavailableView("Select a room", systemImage: "hifispeaker.2",
                                               description: Text("Rooms appear after the daemon can describe the house. A disconnected room never becomes a different target."))
                    }
                }
                .padding(24).frame(maxWidth: 1_100, alignment: .leading)
                .frame(maxWidth: .infinity)
            }
            .toolbar {
                Button("Refresh", systemImage: "arrow.clockwise") { model.refresh() }
                    .disabled(!model.active)
                Button("Disconnect", action: disconnect)
                    .disabled(model.actionPhase == .confirming)
                    .help("Keep this window open for the result of a command already being sent.")
            }
        }
        .sheet(item: Binding(get: { model.review }, set: { if $0 == nil && model.review != nil { model.cancelReview() } })) { review in
            ReviewSheet(review: review, confirm: { model.confirm(review.id) }, cancel: { model.cancelReview() })
        }
    }

    private var households: [String] { Array(Set(model.house?.snapshot?.zones.map(\.household) ?? [])).sorted() }

    private var status: some View {
        VStack(alignment: .leading, spacing: 5) {
            Label(model.active ? (model.house?.isFresh == true ? "Live house state" : "Refreshing / reconnecting — cached observations may be stale") : "Suspended — controls unavailable",
                  systemImage: model.house?.isFresh == true && model.active ? "checkmark.circle" : "arrow.triangle.2.circlepath")
            Text(bridge.endpoint).font(.caption).foregroundStyle(.secondary).textSelection(.enabled)
            if let problem = model.house?.problem { Text(problem).font(.callout).textSelection(.enabled) }
            if let date = model.house?.snapshot?.observedAt {
                Text("Last observation: \(date.formatted(date: .omitted, time: .standard))").font(.caption).foregroundStyle(.secondary)
            }
        }
    }

    @ViewBuilder private func playback(_ zone: Zone) -> some View {
        if let state = model.selectedState {
            GroupBox("Now playing") {
                VStack(alignment: .leading, spacing: 12) {
                    Text(state.track?.displayTitle ?? "No reported track").font(.title2)
                    Text([state.track?.creator, state.track?.album].compactMap { $0 }.joined(separator: " • "))
                        .foregroundStyle(.secondary)
                    HStack {
                        Text(state.transportState.capitalized)
                        Spacer()
                        Button("Previous") { model.prepare(.previous, in: zone) }
                        Button(state.isPlaying ? "Pause" : "Resume") { model.prepare(state.isPlaying ? .pause : .resume, in: zone) }
                            .buttonStyle(.borderedProminent)
                        Button("Next track") { model.prepare(.next, in: zone) }
                    }.disabled(!model.canPrepare)
                    if let track = state.track, let duration = track.durationSecs, duration > 0, let position = track.positionSecs {
                        HStack {
                            Text("Observed \(position)s / \(duration)s").monospacedDigit()
                            TextField("Seconds", text: $seek).frame(width: 90).accessibilityLabel("Seek position in seconds")
                            Button("Review seek") { if let value = UInt32(seek) { model.prepare(.seek(value), in: zone) } }
                                .disabled(!model.canPrepare || UInt32(seek).map { $0 < duration } != true)
                        }
                    }
                }.frame(maxWidth: .infinity, alignment: .leading)
            }
        } else {
            Text(model.house?.snapshot?.failures[zone.id] ?? "The coordinator's playback is not available. Refresh before acting.")
                .textSelection(.enabled)
        }
    }

    private func levels(_ zone: Zone) -> some View {
        GroupBox("Room and group levels") {
            VStack(alignment: .leading, spacing: 12) {
                if zone.members.count > 1, let volume = model.selectedState?.groupVolume {
                    LevelEditor(title: "Whole group", level: volume, enabled: model.canPrepare) { model.prepare(.groupVolume($0), in: zone) }
                        .id("group-" + zone.id.address)
                    Divider()
                }
                ForEach(zone.members, id: \.self) { room in
                    let id = RoomID(household: zone.household, room: room)
                    if let state = model.house?.snapshot?.states[id] {
                        if let volume = state.volume {
                            LevelEditor(title: room, level: volume, enabled: model.canPrepare) { model.prepare(.volume($0, room: room), in: zone) }
                                .id(id)
                        } else { Text("\(room): level unavailable") }
                        if let mute = state.mute {
                            Button(mute ? "Unmute \(room)" : "Mute \(room)") { model.prepare(.mute(!mute, room: room), in: zone) }
                                .disabled(!model.canPrepare)
                        }
                    } else {
                        Text("\(room): \(model.house?.snapshot?.failures[id] ?? "not yet read")").font(.callout)
                    }
                }
                Text("Sliders only choose a target. Review checks the current levels; the daemon enforces its volume policy.")
                    .font(.caption).foregroundStyle(.secondary)
            }.frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    private func layout(_ zone: Zone) -> some View {
        HStack {
            Menu("Group rooms") {
                Button("Group this household") { model.prepare(.party, in: zone) }
                ForEach(zone.members, id: \.self) { room in
                    Menu(room) {
                        if zone.members.count > 1 { Button("Separate \(room)") { model.prepare(.leave(room: room), in: zone) } }
                        ForEach(destinations(zone).filter { $0.household == zone.household }) { target in
                            Button("Join \(room) to \(target.title)") { model.prepare(.join(room: room, to: target.id), in: zone) }
                        }
                    }
                }
            }
            Menu("Move music") {
                ForEach(destinations(zone)) { target in
                    let copy = target.household != zone.household
                    Button("\(copy ? "Replay" : "Move") to \(target.title) [\(target.household)]") {
                        model.prepare(.move(to: target.id, copy: copy), in: zone)
                    }
                }
            }
            Spacer()
        }.disabled(!model.canPrepare)
    }

    private func destinations(_ zone: Zone) -> [Zone] {
        model.house?.snapshot?.zones.filter { !$0.sameGroup(as: zone) } ?? []
    }
}

@MainActor private struct LevelEditor: View {
    let title: String
    let level: Int
    let enabled: Bool
    let submit: (Int) -> Void
    @State private var value: Double
    @State private var editing = false
    init(title: String, level: Int, enabled: Bool, submit: @escaping (Int) -> Void) {
        self.title = title; self.level = level; self.enabled = enabled; self.submit = submit
        _value = State(initialValue: Double(level))
    }
    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack { Text(title).font(.headline); Spacer(); Text("Observed \(level) • target \(Int(value))").monospacedDigit() }
            HStack {
                Slider(value: $value, in: 0...100, step: 1, onEditingChanged: { editing = $0 })
                    .accessibilityLabel("\(title) target volume")
                Button("Review level") { submit(Int(value)) }
            }.disabled(!enabled)
        }
        .onChange(of: level) { _, current in if !editing { value = Double(current) } }
    }
}

@MainActor private struct ReviewSheet: View {
    let review: CommandReview
    let confirm: () -> Void
    let cancel: () -> Void
    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text(review.command.title).font(.title.bold())
            Text("\(review.zone.title) • \(review.zone.household)").font(.headline)
            Text(review.command.explanation)
            requestDetails
            Divider()
            Text("Observed: \(review.state.transportState) • \(review.state.track?.displayTitle ?? "No reported track")")
            if let volume = review.state.volume { Text("Room level: \(volume)") }
            if let favorite = review.favorite { Text("Favorite: \(favorite.title)") }
            if let work = review.dj?.now { Text("DJ selection: \(work.composer) — \(work.title)") }
            Text("The current state will be checked again before sending. This review is not a reservation against another controller. A lost acknowledgement will not be retried.")
                .font(.callout).foregroundStyle(.secondary)
            HStack {
                Button("Cancel", action: cancel).keyboardShortcut(.cancelAction)
                Spacer()
                Button("Confirm once", action: confirm).buttonStyle(.borderedProminent)
            }
        }.padding(28).frame(width: 530)
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
        default: EmptyView()
        }
    }

}
#endif
