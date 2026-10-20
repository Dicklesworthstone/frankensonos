#if os(macOS) || os(iOS)
import SwiftUI
import FrankenSonosKit

@MainActor private final class ListeningSettingsAdapter: ObservableObject {
    let model: ListeningSettingsModel
    init(read: @escaping ListeningSettingsModel.Reader) {
        model = ListeningSettingsModel(read: read)
        model.onChange = { [weak self] in self?.objectWillChange.send() }
    }
}

/// These panels read only while expanded. All changes still use the remote's
/// existing review sheet, command gate, cancellation and outcome presentation.
@MainActor struct SpeakerSoundPanel: View {
    @ObservedObject var bridge: RemoteBridge
    let zone: Zone
    @State private var expanded = false
    @State private var room: String?

    private var chosenRoom: String? {
        if zone.members.count == 1 { return zone.members.first }
        return room.flatMap { zone.members.contains($0) ? $0 : nil }
    }
    var body: some View {
        DisclosureGroup(isExpanded: $expanded) {
            if expanded {
                VStack(alignment: .leading, spacing: 16) {
                    Text("Tune one speaker, without changing the group's music or volume.")
                        .font(.callout).foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                    if zone.members.count > 1 {
                        Picker("Speaker", selection: $room) {
                            Text("Choose a speaker").tag(String?.none)
                            ForEach(zone.members, id: \.self) { Text($0).tag(Optional($0)) }
                        }.pickerStyle(.menu).disabled(!bridge.model.canPrepare)
                    }
                    if let chosenRoom {
                        ListeningSettingsEditor(bridge: bridge, scope: .speaker(zone, room: chosenRoom))
                    } else {
                        Label("Choose the room whose sound you want to adjust.", systemImage: "hifispeaker")
                            .font(.callout).foregroundStyle(.secondary)
                    }
                }.padding(.top, 16)
            }
        } label: {
            Label("Speaker sound", systemImage: "slider.horizontal.3")
                .font(.headline).frame(minHeight: 44)
        }
        .remoteCard()
        .onChange(of: ListeningSettingsScope.order(zone)) { _, _ in room = nil }
    }
}

@MainActor struct PlaybackOrderPanel: View {
    @ObservedObject var bridge: RemoteBridge
    let zone: Zone
    @State private var expanded = false
    var body: some View {
        DisclosureGroup(isExpanded: $expanded) {
            if expanded {
                ListeningSettingsEditor(bridge: bridge, scope: .order(zone)).padding(.top, 16)
            }
        } label: {
            Label("Shuffle & repeat", systemImage: "shuffle")
                .font(.headline).frame(minHeight: 44)
        }.remoteCard()
    }
}

@MainActor private struct ListeningSettingsEditor: View {
    @ObservedObject var bridge: RemoteBridge
    let scope: ListeningSettingsScope
    @StateObject private var adapter: ListeningSettingsAdapter
    @Environment(\.scenePhase) private var scenePhase
    @State private var visible = false
    @State private var actionProblem: String?
    private var settings: ListeningSettingsModel { adapter.model }
    private var available: Bool {
        visible && scenePhase == .active && bridge.model.canPrepare
            && bridge.model.selectedZone?.sameGroup(as: scope.group) == true
    }
    private var editable: Bool { available && settings.canEdit && settings.scope == scope }

    init(bridge: RemoteBridge, scope: ListeningSettingsScope) {
        self.bridge = bridge; self.scope = scope
        _adapter = StateObject(wrappedValue: ListeningSettingsAdapter(read: bridge.settingsReader))
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            Text(scope.target.address).font(.subheadline.weight(.medium))
                .fixedSize(horizontal: false, vertical: true)
            readStatus
            if let problem = settings.problem {
                Label(problem, systemImage: "exclamationmark.circle")
                    .font(.callout).foregroundStyle(.secondary).textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if let actionProblem {
                Label(actionProblem, systemImage: "exclamationmark.circle")
                    .font(.callout).foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if let tone = settings.tone { toneControls(tone) }
            if let order = settings.order { orderControls(order) }
            if let notice = settings.notice {
                Text(notice).font(.callout).foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if settings.observed != nil {
                if settings.hasEdits {
                    Label("Not applied yet", systemImage: "pencil.circle")
                        .font(.caption.weight(.medium)).foregroundStyle(RemoteStyle.accent)
                }
                ViewThatFits(in: .horizontal) {
                    HStack(spacing: 12) { draftActions }
                    VStack(alignment: .leading, spacing: 12) { draftActions }
                }
                Text("Adjustments stay here until you review and confirm. Choosing another speaker discards unsaved edits.")
                    .font(.caption).foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .onAppear { visible = true; reconcile() }
        .onDisappear { visible = false; settings.suspend() }
        .onChange(of: scope) { _, _ in actionProblem = nil; reconcile() }
        .onChange(of: bridge.model.canPrepare) { _, _ in reconcile() }
        .onChange(of: scenePhase) { _, _ in reconcile() }
    }

    @ViewBuilder private var readStatus: some View {
        if settings.loading {
            HStack(spacing: 10) { ProgressView().controlSize(.small); Text("Reading current setting…") }
                .font(.callout).foregroundStyle(.secondary)
        } else {
            ViewThatFits(in: .horizontal) {
                HStack(spacing: 12) { observationTime; Spacer(minLength: 0); refreshButton }
                VStack(alignment: .leading, spacing: 8) { observationTime; refreshButton }
            }
        }
        if !available {
            Text("Finish the current review or action before making another adjustment.")
                .font(.caption).foregroundStyle(.secondary)
        }
    }
    private var observationTime: some View {
        Group {
            if let date = settings.observedAt {
                Text("\(settings.isFresh ? "Read" : "Last read") at \(date.formatted(date: .omitted, time: .standard))")
            } else { Text("No setting observed yet") }
        }.font(.caption).foregroundStyle(.secondary)
    }
    private var refreshButton: some View {
        Button { actionProblem = nil; settings.refresh() } label: {
            Label("Read again", systemImage: "arrow.clockwise").frame(minHeight: 32)
        }.buttonStyle(.bordered).disabled(!available || !settings.canRefresh)
            .accessibilityHint("Read the selected speaker or group. No setting is changed.")
    }

    private func toneControls(_ tone: SpeakerTone) -> some View {
        VStack(alignment: .leading, spacing: 20) {
            toneSlider("Bass", value: tone.bass) { settings.editTone(bass: $0) }
            toneSlider("Treble", value: tone.treble) { settings.editTone(treble: $0) }
            Toggle(isOn: Binding(get: { settings.tone?.loudness ?? tone.loudness },
                                 set: { if editable { settings.editTone(loudness: $0) } })) {
                VStack(alignment: .leading, spacing: 4) {
                    Text("Loudness").font(.headline)
                    Text("Compensates bass and treble at low listening levels. It does not change the volume.")
                        .font(.caption).foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            .toggleStyle(.switch).disabled(!editable)
            Button("Choose neutral sound", systemImage: "arrow.counterclockwise") {
                guard editable else { return }
                settings.editTone(bass: 0, treble: 0, loudness: false)
            }.buttonStyle(.bordered).disabled(!editable)
                .accessibilityHint("Prepares zero bass, zero treble and loudness off. Review before applying.")
        }
    }

    private func toneSlider(_ title: String, value: Int, edit: @escaping (Int) -> Void) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                Text(title).font(.headline)
                Spacer()
                Text(SpeakerTone.level(value)).font(.body.monospacedDigit()).foregroundStyle(.secondary)
            }
            Slider(value: Binding(get: { Double(value) }, set: { chosen in
                guard editable, chosen.isFinite, (-10...10).contains(chosen) else { return }
                edit(Int(chosen.rounded()))
            }), in: -10...10, step: 1) { Text(title) }
                .frame(minHeight: 44).disabled(!editable).accessibilityValue(SpeakerTone.level(value))
                .accessibilityHint("Local adjustment from minus ten to plus ten. Review changes to apply.")
        }
    }

    private func orderControls(_ order: QueuePlaybackOrder) -> some View {
        VStack(alignment: .leading, spacing: 16) {
            if let limitation = order.limitation {
                Label(limitation, systemImage: order.djActive ? "music.note.list" : "info.circle")
                    .font(.callout).foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Toggle("Shuffle", isOn: Binding(get: { settings.order?.mode.shuffled ?? order.mode.shuffled },
                                           set: { if editable { settings.editOrder(shuffled: $0) } }))
                .toggleStyle(.switch).disabled(!editable || !order.queueActive || order.djActive)
            Picker("Repeat", selection: Binding(get: { settings.order?.mode.repetition ?? order.mode.repetition },
                                                set: { if editable { settings.editOrder(repetition: $0) } })) {
                ForEach(QueuePlaybackMode.Repetition.allCases, id: \.self) { Text($0.title).tag($0) }
            }.pickerStyle(.menu).disabled(!editable || !order.queueActive || order.djActive)
            if order.djActive && order.queueActive && order.mode != .normal {
                Button("Use sequential playback", systemImage: "list.number") {
                    guard editable else { return }
                    settings.useSequentialOrder()
                }.buttonStyle(.bordered).disabled(!editable)
            }
            Text(order.mode.summary).font(.subheadline.weight(.medium))
                .fixedSize(horizontal: false, vertical: true)
            Text("Changes how the existing queue plays. Does not start paused music, switch sources, or rearrange saved queue entries.")
                .font(.caption).foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    @ViewBuilder private var draftActions: some View {
        let selection = settings.selection()
        Button {
            guard let selection else { return }
            do {
                guard available, selection.scope == scope else {
                    throw ContractError.invalid("The destination or controls changed. Read and review the current setting again.")
                }
                let change = try settings.resolve(selection)
                actionProblem = nil
                // The immutable command now owns the draft. A later read must
                // not call our acknowledged change an unsent external edit.
                settings.discard()
                bridge.model.prepare(change.command, in: selection.scope.group)
            } catch { actionProblem = error.localizedDescription }
        } label: {
            Label("Review changes", systemImage: "checkmark.circle").frame(minHeight: 32)
        }
        .buttonStyle(.borderedProminent).foregroundStyle(RemoteStyle.accentInk)
        .disabled(!editable || selection == nil)
        Button("Discard edits") {
            guard editable else { return }
            actionProblem = nil; settings.discard()
        }.buttonStyle(.bordered).disabled(!editable || !settings.hasEdits)
    }

    private func reconcile() {
        if available { settings.activate(scope) }
        else { settings.suspend() }
    }
}
#endif
