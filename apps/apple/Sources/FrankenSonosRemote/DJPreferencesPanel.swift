#if os(macOS) || os(iOS)
import SwiftUI
import FrankenSonosKit

/// Standing taste is a daemon setting, not the selected room's temporary steer.
@MainActor struct DJPreferencesPanel: View {
    @ObservedObject var bridge: RemoteBridge
    @State private var expanded = false
    @State private var family = "favor"
    @State private var key: DJPreferenceKey = .favorGenres
    @State private var entry = ""
    @State private var filter = ""
    @State private var followDay = true
    @State private var energy = 50.0
    @State private var allowExplicit = false
    @State private var previous: DJPreferences?
    @State private var problem: String?
    private var remote: RemoteModel { bridge.model }
    private var workflow: DJPreferencesWorkflow { remote.preferences }
    private var editor: DJPreferencesModel { workflow.editor }

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            DisclosureGroup(isExpanded: $expanded) {
                VStack(alignment: .leading, spacing: 20) {
                    Text("Teach every DJ what feels like you. These choices last beyond this session; temporary room steering still takes precedence.")
                        .font(.callout).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                    status
                    if let observed = editor.observation {
                        energyControls(observed)
                        explicitControls(observed)
                        listControls(observed)
                    }
                    if let problem {
                        Label(problem, systemImage: "exclamationmark.circle")
                            .font(.callout).foregroundStyle(.secondary).textSelection(.enabled)
                    }
                }.padding(.top, 16)
            } label: {
                HStack(alignment: .top, spacing: 12) {
                    Image(systemName: "heart.text.square").font(.title2).foregroundStyle(RemoteStyle.accent)
                        .accessibilityHidden(true)
                    VStack(alignment: .leading, spacing: 4) {
                        Text("Your standing taste").font(.headline)
                        Text("Every group · future DJ picks").font(.caption).foregroundStyle(.secondary)
                    }
                }.frame(minHeight: 44)
            }
            if let result = workflow.result { resultNotice(result) }
        }
        .remoteCard()
        .onChange(of: expanded) { _, _ in updateVisibility() }
        .onChange(of: remote.active) { _, _ in updateVisibility() }
        .onChange(of: editor.observation?.id) { _, _ in adoptObservation() }
        .onAppear { adoptObservation(); updateVisibility() }
        .onDisappear { workflow.suspend() }
        .sheet(item: Binding(get: { workflow.review }, set: { if $0 == nil && workflow.review != nil { workflow.cancel() } })) { review in
            DJPreferenceReviewSheet(review: review,
                                    confirm: { workflow.confirm(review.id) }, cancel: { workflow.cancel() })
        }
    }

    @ViewBuilder private var status: some View {
        if workflow.phase != .idle || workflow.isEngaged {
            HStack(spacing: 12) {
                ProgressView().controlSize(.small)
                Text(workflow.phase == .confirming ? "Saving one preference…" : "Checking your preference review…")
                    .font(.callout)
                Spacer(minLength: 8)
                Button("Cancel") { workflow.cancel() }
                    .help("Stops unsent work. A submitted save may already have taken effect.")
            }
        }
        if editor.loading { ProgressView("Reading standing taste…") }
        if let message = editor.problem {
            Label(message, systemImage: "wifi.exclamationmark").font(.callout).textSelection(.enabled)
        }
        if workflow.needsRefresh {
            Label("Read current taste before making another change. Reopening this panel does not retry or confirm the save.", systemImage: "exclamationmark.circle")
                .font(.callout).fixedSize(horizontal: false, vertical: true)
        }
        ViewThatFits(in: .horizontal) {
            HStack { observationTime; Spacer(); refreshButton }
            VStack(alignment: .leading, spacing: 8) { observationTime; refreshButton }
        }
    }

    @ViewBuilder private var observationTime: some View {
        if let observed = editor.observation {
            Text("\(editor.fresh ? "Read" : "Last read") \(observed.observedAt.formatted(date: .abbreviated, time: .shortened))")
                .font(.caption).foregroundStyle(.secondary)
        } else { Text("No preferences read yet").font(.caption).foregroundStyle(.secondary) }
    }
    private var refreshButton: some View {
        Button("Refresh standing taste", systemImage: "arrow.clockwise") { workflow.refresh() }
            .buttonStyle(.bordered).disabled(!remote.active || workflow.isEngaged || !remote.canSchedule || editor.loading)
            .frame(minHeight: 44)
    }

    private func energyControls(_ observed: DJPreferencesObservation) -> some View {
        VStack(alignment: .leading, spacing: 12) {
            Label("Energy through the day", systemImage: "sun.max").font(.headline)
            Text(observed.preferences.energy.map { "Saved default: \($0) of 100" } ?? "Saved default: follow the time of day")
                .font(.caption).foregroundStyle(.secondary)
            Toggle("Follow the time of day", isOn: $followDay).disabled(!workflow.canPrepare)
            if !followDay {
                HStack {
                    Text("Standing energy").font(.subheadline)
                    Spacer()
                    Text("\(Int(energy)) / 100").font(.subheadline.monospacedDigit())
                }
                Slider(value: $energy, in: 0...100, step: 1) { Text("Standing DJ energy") }
                    .accessibilityValue("\(Int(energy)) of 100").disabled(!workflow.canPrepare)
                HStack { Text("Gentle"); Spacer(); Text("Energetic") }.font(.caption).foregroundStyle(.secondary)
            }
            Button("Review energy change") {
                prepare(from: observed) { try DJPreferenceChange(key: .energy, value: String(Int(energy)), unset: followDay) }
            }.buttonStyle(.bordered).frame(minHeight: 44)
                .disabled(!workflow.canPrepare || (followDay ? observed.preferences.energy == nil : observed.preferences.energy == Int(energy)))
            Text("This is musical energy, not speaker volume. A temporary energy steer takes priority.")
                .font(.caption).foregroundStyle(.secondary)
        }
    }

    private func explicitControls(_ observed: DJPreferencesObservation) -> some View {
        VStack(alignment: .leading, spacing: 12) {
            Divider()
            Label("Explicit tracks", systemImage: "music.note").font(.headline)
            Toggle("Allow explicit tracks in future DJ picks", isOn: $allowExplicit).disabled(!workflow.canPrepare)
            Text(observed.preferences.explicit ? "Currently allowed by standing taste." : "Currently kept out by standing taste.")
                .font(.caption).foregroundStyle(.secondary)
            Button("Review explicit-content change") {
                prepare(from: observed) { try DJPreferenceChange(key: .explicit, value: allowExplicit ? "true" : "false") }
            }.buttonStyle(.bordered).frame(minHeight: 44)
                .disabled(!workflow.canPrepare || allowExplicit == observed.preferences.explicit)
        }
    }

    private func listControls(_ observed: DJPreferencesObservation) -> some View {
        VStack(alignment: .leading, spacing: 14) {
            Divider()
            Label("Make it personal", systemImage: "slider.horizontal.3").font(.headline)
            Picker("Preference strength", selection: $family) {
                Text("Favor").tag("favor"); Text("Avoid").tag("avoid")
                Text("Pin").tag("pin"); Text("Ban").tag("ban")
            }.pickerStyle(.menu).disabled(workflow.isEngaged)
                .onChange(of: family) { _, chosen in
                    if key.group != chosen, let first = DJPreferenceKey.lists.first(where: { $0.group == chosen }) { key = first }
                    filter = ""
                }
            Text(familyExplanation).font(.callout).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            Picker("Kind of preference", selection: $key) {
                ForEach(DJPreferenceKey.lists.filter { $0.group == family }, id: \.self) { value in
                    Text(value.category.capitalized).tag(value)
                }
            }.pickerStyle(.menu).disabled(workflow.isEngaged)
            TextField(entryPlaceholder, text: $entry, axis: .vertical)
                .textFieldStyle(.roundedBorder).lineLimit(1...4)
                .accessibilityLabel("One \(key.category) preference")
                .disabled(!workflow.canPrepare)
            Text("One value, up to 256 characters. Commas stay part of the name. Artist names, genres and periods are interpreted by the daemon.")
                .font(.caption).foregroundStyle(.secondary)
            Button("Review addition", systemImage: "plus") {
                prepare(from: observed) { try DJPreferenceChange(key: key, value: entry) }
            }.buttonStyle(.borderedProminent).frame(minHeight: 44)
                .disabled(!workflow.canPrepare || (try? DJPreferenceChange(key: key, value: entry)) == nil)
            savedEntries(observed)
        }
    }

    private func savedEntries(_ observed: DJPreferencesObservation) -> some View {
        let selectedKey = key
        let all = observed.preferences.entries(for: selectedKey)
        let shown = all.enumerated().filter { filter.isEmpty || $0.element.localizedStandardContains(filter) }
        return VStack(alignment: .leading, spacing: 10) {
            Text("Saved \(selectedKey.title.lowercased()) · \(all.count)").font(.subheadline.weight(.semibold))
            if all.count > 8 {
                TextField("Filter saved entries", text: $filter).textFieldStyle(.roundedBorder)
                    .accessibilityLabel("Filter saved \(selectedKey.title.lowercased())")
            }
            if all.isEmpty { Text("None stated. Your library and feedback still guide the DJ.").font(.callout).foregroundStyle(.secondary) }
            else if shown.isEmpty { Text("No matching entries.").font(.callout).foregroundStyle(.secondary) }
            LazyVStack(alignment: .leading, spacing: 8) {
                ForEach(shown, id: \.offset) { item in
                    HStack(alignment: .top, spacing: 12) {
                        Text(item.element).font(.callout).fixedSize(horizontal: false, vertical: true).textSelection(.enabled)
                            .frame(maxWidth: .infinity, alignment: .leading)
                        Button {
                            prepare(from: observed) { try DJPreferenceChange(key: selectedKey, value: item.element, unset: true) }
                        } label: { Image(systemName: "minus.circle").frame(width: 44, height: 44) }
                            .buttonStyle(.borderless).disabled(!workflow.canPrepare)
                            .accessibilityLabel("Review removing \(item.element) from \(selectedKey.title.lowercased())")
                    }.padding(.vertical, 4)
                }
            }
        }
    }

    private var familyExplanation: String {
        switch family {
        case "favor": "Give matching music more weight without excluding the rest of your collection."
        case "avoid": "Prefer something else. The DJ may relax an avoid when no alternatives remain and explains that choice."
        case "pin": "Strongly favor particular artists, albums or tracks. Pinning does not add them to your queue."
        default: "Exclude particular artists, albums or tracks without automatic relaxation. Explicit temporary steering takes precedence over standing taste."
        }
    }
    private var entryPlaceholder: String {
        switch key.category {
        case "eras": "A decade or period, such as 1960s or baroque"
        case "moods": "An existing DJ mood name"
        case "albums": "An exact album title or Spotify album URI"
        case "tracks": "An exact track title or Spotify track URI"
        case "genres": "A genre, such as jazz"
        default: "One artist or composer name"
        }
    }

    private func resultNotice(_ result: DJPreferencesResult) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            Divider()
            HStack(alignment: .top) {
                Label(result.title, systemImage: result.kind == .uncertain || result.kind == .notSent ? "exclamationmark.circle" : "checkmark.circle")
                    .font(.headline)
                Spacer(minLength: 8)
                Button { workflow.dismissResult() } label: { Image(systemName: "xmark").frame(width: 44, height: 44) }
                    .buttonStyle(.plain).accessibilityLabel("Dismiss preference result")
            }
            Text(DJPreferenceChange.scope).font(.caption).foregroundStyle(.secondary)
            Text(result.change.title).font(.callout.weight(.medium)).fixedSize(horizontal: false, vertical: true)
            Text(result.detail).font(.callout).textSelection(.enabled).fixedSize(horizontal: false, vertical: true)
        }.accessibilityElement(children: .contain)
    }

    private func prepare(from observed: DJPreferencesObservation, change: () throws -> DJPreferenceChange) {
        do { let value = try change(); problem = nil; workflow.prepare(value, from: observed) }
        catch { problem = error.localizedDescription }
    }
    private func updateVisibility() {
        if expanded && remote.active { workflow.resume() } else { workflow.suspend() }
    }
    private func adoptObservation() {
        guard editor.fresh, let value = editor.observation?.preferences else { return }
        if previous == nil || previous?.energy != value.energy {
            if previous != nil { problem = "The saved energy changed; its local draft was reset. Your unsent list entry is unchanged." }
            followDay = value.energy == nil; energy = Double(value.energy ?? 50)
        }
        if previous == nil || previous?.explicit != value.explicit { allowExplicit = value.explicit }
        previous = value
    }
}

@MainActor private struct DJPreferenceReviewSheet: View {
    let review: DJPreferenceReview
    let confirm: () -> Void
    let cancel: () -> Void
    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 18) {
                    Label("Every DJ, not just this room", systemImage: "hifispeaker.2")
                        .font(.headline).foregroundStyle(RemoteStyle.accent)
                    Text(review.command.change.title).font(.title2.bold()).fixedSize(horizontal: false, vertical: true)
                    Text(DJPreferenceChange.explanation).font(.callout)
                    Divider()
                    Text("Before this change").font(.headline)
                    baseline
                    Text("Preferences read \(review.observedAt.formatted(date: .abbreviated, time: .shortened))")
                        .font(.caption).foregroundStyle(.secondary)
                    Text("The complete preferences will be checked again before saving one key. This is not an atomic reservation against other controllers. A lost acknowledgement will not be retried.")
                        .font(.callout).foregroundStyle(.secondary)
                }.padding(24).frame(maxWidth: .infinity, alignment: .leading)
            }
            .navigationTitle("Review standing taste")
            #if os(iOS)
            .navigationBarTitleDisplayMode(.inline)
            #endif
            .safeAreaInset(edge: .bottom) {
                VStack(spacing: 10) {
                    Button("Save for every DJ", action: confirm).buttonStyle(.borderedProminent)
                        .frame(minHeight: 44)
                    Button("Cancel", action: cancel).buttonStyle(.bordered).frame(minHeight: 44)
                }.frame(maxWidth: .infinity).padding(16).background(.regularMaterial)
            }
        }
        #if os(macOS)
        .frame(width: 560, height: 620)
        #endif
        .presentationDetents([.large]).presentationDragIndicator(.visible).interactiveDismissDisabled()
    }
    @ViewBuilder private var baseline: some View {
        let value = review.command.before, change = review.command.change
        if change.key == .energy {
            Text(value.energy.map { "Energy: \($0) of 100" } ?? "Energy follows the time of day.")
        } else if change.key == .explicit {
            Text(value.explicit ? "Explicit tracks allowed." : "Explicit tracks kept out.")
        } else {
            Text("\(value.entries(for: change.key).count) entries in \(change.key.title.lowercased()). Only the named entry changes; this does not clear the list.")
            if let item = change.value { Text(item).font(.callout.monospaced()).textSelection(.enabled) }
        }
    }
}
#endif
