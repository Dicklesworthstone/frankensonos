#if os(macOS) || os(iOS)
import SwiftUI
import FrankenSonosKit

/// Temporary session edits reuse the playback review sheet and writer. No
/// network operation is dispatched by a text field, picker, or toggle.
@MainActor struct DJSteeringPanel: View {
    @ObservedObject var bridge: RemoteBridge
    let zone: Zone
    let status: DJStatus
    @State private var draft: DJSteeringDraft?
    @State private var problem: String?
    @State private var reloadPending = false
    private var model: RemoteModel { bridge.model }
    private var ready: Bool { model.canPrepare && !model.panelLoading && model.panelProblem == nil
        && model.selectedZone?.sameGroup(as: zone) == true }
    private var unchanged: Bool {
        guard let draft, draft.group.sameGroup(as: zone) else { return false }
        do { try draft.clear().validate(status); return true } catch { return false }
    }

    var body: some View {
        DisclosureGroup {
            VStack(alignment: .leading, spacing: 16) {
                Text("Steer this group’s future DJ picks by artist, genre, decade or work length. This replaces temporary session settings, not your standing taste or already queued music.")
                    .font(.callout).foregroundStyle(.secondary)
                Text("Observed source: \(status.steering.source) · \(zone.household)")
                    .font(.caption).foregroundStyle(.secondary)
                Button("Refresh DJ and reload these settings", systemImage: "arrow.clockwise") {
                    reloadPending = true; draft = nil; problem = nil; model.refreshPanel()
                }.buttonStyle(.bordered).disabled(!ready)
                if let draft {
                    editor(draft)
                    if !unchanged {
                        Text("The session or group changed. Refresh and reload before reviewing another change.")
                            .font(.callout).foregroundStyle(.secondary)
                    }
                } else if model.panelLoading {
                    ProgressView("Reading current DJ settings…")
                }
                if let problem { Text(problem).font(.callout).foregroundStyle(.secondary).textSelection(.enabled) }
                if let relaxed = status.now?.reason.relaxed, !relaxed.isEmpty {
                    Text("The current pick relaxed these filters: \(relaxed.joined(separator: ", ")).")
                        .font(.caption).foregroundStyle(.secondary)
                }
                Text("The daemon decides what the library can support and may report relaxed filters when too few works match. Saving does not start a stopped DJ.")
                    .font(.caption).foregroundStyle(.secondary)
            }.padding(.top, 16)
        } label: {
            Label("Temporary session filters", systemImage: "slider.horizontal.3")
                .font(.headline).frame(minHeight: 44)
        }
        .remoteCard()
        .onAppear { loadObserved() }
        .onChange(of: model.panelLoading) { _, loading in
            if !loading && (reloadPending || draft == nil) { reloadPending = false; loadObserved() }
        }
        .onChange(of: zone) { old, current in
            if !old.sameGroup(as: current) { draft = nil; problem = "Choose this group’s settings again." }
        }
    }

    private func editor(_ observed: DJSteeringDraft) -> some View {
        VStack(alignment: .leading, spacing: 16) {
            Picker("Base mood", selection: binding(\.mood, fallback: "")) {
                Text("Automatic program").tag("")
                if !observed.mood.isEmpty && !model.moods.contains(where: { $0.name == observed.mood }) {
                    Text("Unavailable: \(observed.mood)").tag(observed.mood)
                }
                ForEach(model.moods) { Text($0.name).tag($0.name) }
            }.pickerStyle(.menu)
            Text("One name per line. Commas remain part of an artist or keyword. Up to 32 entries per filter.")
                .font(.caption).foregroundStyle(.secondary)
            filterFields([.includeArtists, .excludeArtists, .includeGenres, .excludeGenres])
            DisclosureGroup("Decades and keywords") {
                filterFields([.decades, .excludeDecades, .includeKeywords, .excludeKeywords]).padding(.top, 12)
                Text("Write decades as 1960 or 1980, not 60s or 80s.").font(.caption).foregroundStyle(.secondary)
            }
            DisclosureGroup("Classical works") {
                filterFields([.includeComposers, .excludeComposers, .periods, .excludePeriods]).padding(.top, 12)
                Text("Periods: \(DJSteeringFilters.periods.joined(separator: ", ")).")
                    .font(.caption).foregroundStyle(.secondary)
            }
            HStack {
                numberField("Minimum work minutes", value: binding(\.minimumMinutes, fallback: ""))
                numberField("Maximum work minutes", value: binding(\.maximumMinutes, fallback: ""))
            }
            Stepper("Energy shift: \(observed.energyBias)", value: binding(\.energyBias, fallback: 0), in: -2...2)
            Text("−2 is much calmer; +2 is much brighter. Zero adds no shift to the base mood.")
                .font(.caption).foregroundStyle(.secondary)
            Toggle("Allow long works", isOn: binding(\.allowLongWorks, fallback: false))
            Divider()
            Toggle("Keep until explicitly cleared", isOn: binding(\.untilCleared, fallback: false))
            if !observed.untilCleared {
                numberField("New duration in minutes (1–10080)", value: binding(\.minutes, fallback: "60"))
            }
            Text("The replacement gets the duration shown here starting when saved; it does not preserve the old deadline.")
                .font(.caption).foregroundStyle(.secondary)
            Button("Review replacement", systemImage: "checkmark.circle") { review(clear: false) }
                .buttonStyle(.borderedProminent).foregroundStyle(RemoteStyle.accentInk)
                .disabled(!unchanged)
            Button("Review clearing all temporary steering", systemImage: "arrow.uturn.backward") { review(clear: true) }
                .buttonStyle(.bordered).disabled(!unchanged)
        }.disabled(!ready)
    }

    private func filterFields(_ fields: [DJSteeringField]) -> some View {
        VStack(alignment: .leading, spacing: 12) {
            ForEach(fields) { field in
                VStack(alignment: .leading, spacing: 5) {
                    Text(field.title).font(.subheadline.weight(.medium))
                    TextField(field.title, text: Binding(get: { draft?.entries[field] ?? "" }, set: { value in
                        guard ready else { return }; draft?.entries[field] = value; problem = nil
                    }), axis: .vertical)
                    .lineLimit(2...5).textFieldStyle(.roundedBorder).accessibilityLabel(field.title)
                    #if os(iOS)
                    .autocorrectionDisabled()
                    #endif
                }
            }
        }
    }
    private func numberField(_ title: String, value: Binding<String>) -> some View {
        VStack(alignment: .leading, spacing: 5) {
            Text(title).font(.caption).foregroundStyle(.secondary)
            TextField("Optional", text: value).textFieldStyle(.roundedBorder).accessibilityLabel(title)
                #if os(iOS)
                .keyboardType(.numberPad)
                #endif
        }
    }
    private func binding<Value>(_ key: WritableKeyPath<DJSteeringDraft, Value>, fallback: Value) -> Binding<Value> {
        Binding(get: { draft?[keyPath: key] ?? fallback }, set: { value in
            guard ready else { return }; draft?[keyPath: key] = value; problem = nil
        })
    }
    private func loadObserved() {
        guard !model.panelLoading && model.panelProblem == nil else { return }
        do { draft = try DJSteeringDraft(in: zone, status: status); problem = nil }
        catch { draft = nil; problem = error.localizedDescription }
    }
    private func review(clear: Bool) {
        guard ready && unchanged, let draft else { return }
        do {
            let change = try clear ? draft.clear() : draft.change(moods: model.moods)
            problem = nil; model.prepare(.djFilters(change), in: zone)
        } catch { problem = error.localizedDescription }
    }
}
#endif
