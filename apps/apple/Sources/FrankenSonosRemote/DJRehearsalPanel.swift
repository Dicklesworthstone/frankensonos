#if os(macOS) || os(iOS)
import SwiftUI
import FrankenSonosKit

/// Generate and export stay read-only. A separate explicit handoff can open
/// the existing review for temporary settings; it never starts playback.
@MainActor struct DJRehearsalPanel: View {
    @ObservedObject var bridge: RemoteBridge
    @State private var expanded = false
    private var rehearsal: DJRehearsalModel { bridge.rehearsal }
    private var remote: RemoteModel { bridge.model }
    private var zone: Zone? { remote.selectedZone }
    private var canEdit: Bool { remote.active && rehearsal.active }

    var body: some View {
        DisclosureGroup(isExpanded: $expanded) {
            VStack(alignment: .leading, spacing: 18) {
                Text("See the set your DJ would choose, without playing, queuing, saving preferences or recording listening history.")
                    .font(.callout).foregroundStyle(.secondary)
                if let zone {
                    Text("History and steering from: \(zone.title) · \(zone.household)")
                        .font(.subheadline.weight(.medium))
                }
                choices.disabled(!canEdit)
                if let problem = rehearsal.draftProblem {
                    Label(problem, systemImage: "info.circle").font(.callout).foregroundStyle(.secondary)
                }
                HStack(spacing: 12) {
                    Button {
                        guard remote.active, expanded else { return }
                        rehearsal.activate(in: zone)
                        rehearsal.generate()
                    } label: {
                        Label("Generate preview", systemImage: "list.bullet.rectangle")
                            .frame(minHeight: 32)
                    }.buttonStyle(.borderedProminent).foregroundStyle(RemoteStyle.accentInk)
                        .disabled(!canEdit || !rehearsal.canGenerate || rehearsal.draftProblem != nil)
                        .accessibilityHint("Reads a hypothetical set from the daemon. Does not start music.")
                    if rehearsal.loading {
                        Button("Cancel preview") { rehearsal.cancel() }.buttonStyle(.bordered)
                    }
                }
                if rehearsal.loading {
                    ProgressView("Generating preview…").accessibilityLabel("Generating DJ preview")
                }
                if let problem = rehearsal.problem {
                    Label(problem, systemImage: "exclamationmark.circle")
                        .font(.callout).textSelection(.enabled)
                }
                if let observation = rehearsal.observation { results(observation) }
                Text("The same seed reproduces a set only while the library, history, preferences, steering and start time stay the same. A later live session may choose differently.")
                    .font(.caption).foregroundStyle(.secondary)
            }.padding(.top, 16)
        } label: {
            Label("Preview a DJ set", systemImage: "list.bullet.rectangle")
                .font(.headline).frame(minHeight: 44)
        }
        .remoteCard()
        .onAppear { synchronize() }
        .onChange(of: expanded) { _, _ in synchronize() }
        .onChange(of: zone) { _, _ in synchronize() }
        .onChange(of: remote.active) { _, _ in synchronize() }
        .onDisappear { rehearsal.suspend() }
    }

    private var choices: some View {
        VStack(alignment: .leading, spacing: 14) {
            Picker("Preview length", selection: binding(\.length)) {
                Text("Works").tag(DJRehearsalDraft.Length.works)
                Text("Hours").tag(DJRehearsalDraft.Length.hours)
            }.pickerStyle(.segmented)
            if rehearsal.draft.length == .works {
                Stepper("\(rehearsal.draft.works) works", value: binding(\.works), in: 1...500)
            } else {
                Stepper("\(rehearsal.draft.hours) hours", value: binding(\.hours), in: 1...48)
                Text("Includes whole works starting within this window, up to 500 works. The final work can finish later.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            Picker("Preview starts", selection: binding(\.start)) {
                Text("Now").tag(DJRehearsalDraft.Start.now)
                Text("House time").tag(DJRehearsalDraft.Start.houseTime)
            }.pickerStyle(.segmented)
            if rehearsal.draft.start == .houseTime {
                TextField("HH:MM", text: binding(\.houseTime))
                    .textFieldStyle(.roundedBorder).accessibilityLabel("Preview start in house time, HH:MM")
                    #if os(iOS)
                    .keyboardType(.numbersAndPunctuation)
                    #endif
                Text("The next occurrence of this time on the daemon’s house clock, not your phone’s time zone.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            TextField("Variation seed", text: binding(\.seed))
                .textFieldStyle(.roundedBorder).accessibilityLabel("Preview variation seed")
                #if os(iOS)
                .keyboardType(.numberPad)
                #endif
            DisclosureGroup("Preview-only mood and filters") {
                VStack(alignment: .leading, spacing: 14) {
                    Text("Leave all choices empty to follow the group’s existing steering and its expiry. Any mood or filter here replaces that steering only for this preview. Standing taste still applies.")
                        .font(.caption).foregroundStyle(.secondary)
                    TextField("Optional mood name", text: binding(\.mood))
                        .textFieldStyle(.roundedBorder).accessibilityLabel("Preview-only mood")
                        #if os(iOS)
                        .autocorrectionDisabled()
                        .textInputAutocapitalization(.never)
                        #endif
                    if !remote.moods.isEmpty {
                        Menu("Choose an observed mood") {
                            Button("No preview mood") { edit(\.mood, "") }
                            ForEach(remote.moods) { mood in
                                Button(mood.name) { edit(\.mood, mood.name) }
                            }
                        }.buttonStyle(.bordered)
                    }
                    Text("One value per line; commas remain part of a name.")
                        .font(.caption).foregroundStyle(.secondary)
                    filterFields([.includeArtists, .excludeArtists, .includeGenres, .excludeGenres])
                    DisclosureGroup("Decades and keywords") {
                        filterFields([.decades, .excludeDecades, .includeKeywords, .excludeKeywords]).padding(.top, 10)
                        Text("Use decade start years such as 1960 or 1980.").font(.caption).foregroundStyle(.secondary)
                    }
                    DisclosureGroup("Classical works") {
                        filterFields([.includeComposers, .excludeComposers, .periods, .excludePeriods]).padding(.top, 10)
                        Text("Periods: \(DJRehearsalDraft.periods.joined(separator: ", ")).")
                            .font(.caption).foregroundStyle(.secondary)
                    }
                    numberField("Minimum work minutes", text: binding(\.minimumMinutes))
                    numberField("Maximum work minutes", text: binding(\.maximumMinutes))
                    Stepper("Energy shift: \(rehearsal.draft.energyBias)", value: binding(\.energyBias), in: -2...2)
                    Toggle("Allow long works", isOn: binding(\.allowLongWorks))
                    Button("Remove all preview-only overrides") {
                        var draft = rehearsal.draft
                        draft.mood = ""; draft.filters = [:]; draft.minimumMinutes = ""; draft.maximumMinutes = ""
                        draft.energyBias = 0; draft.allowLongWorks = false
                        rehearsal.update(draft)
                    }.buttonStyle(.bordered)
                }.padding(.top, 12)
            }
        }
    }

    private func results(_ observation: DJRehearsalObservation) -> some View {
        let report = observation.report
        return VStack(alignment: .leading, spacing: 16) {
            Divider()
            Label(rehearsal.matchesChoices ? "Generated preview — not a live queue" : "Previous preview — generate again for current choices",
                  systemImage: rehearsal.matchesChoices ? "list.bullet" : "clock.arrow.circlepath")
                .font(.headline)
            Text("\(observation.request.zone.title) · \(observation.request.zone.household)")
                .font(.subheadline)
            Text("House start: \(report.from)").font(.callout.monospaced()).textSelection(.enabled)
            Text("Generated \(observation.generatedAt.formatted(date: .abbreviated, time: .shortened)) on this device")
                .font(.caption).foregroundStyle(.secondary)
            Text("\(report.summary.works) works · \(report.summary.minutes) planned minutes · \(report.summary.distinctArtists) artists")
                .font(.subheadline.weight(.medium))
            Text("\(report.summary.explicit) explicit works · \(report.relaxedWorkCount) picks with relaxed filters")
                .font(.caption).foregroundStyle(.secondary)
            Text(observation.request.scopeDescription).font(.caption).foregroundStyle(.secondary)
            ShareLink(item: report.text(for: observation.request)) {
                Label("Share this preview as text", systemImage: "square.and.arrow.up")
            }.buttonStyle(.bordered)
                .accessibilityHint("Exports this generated observation, not the current draft or a playable queue.")
            if observation.request.overridesSteering {
                RehearsalSteeringControls(bridge: bridge, observation: observation)
                    .id(observation.id)
            } else {
                Text("This preview inherited the group’s steering. Add explicit preview-only settings and generate again to review a new temporary session.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            if report.works.isEmpty {
                Text("The daemon returned no works for this preview. Change the choices or check your cached library.")
                    .font(.callout)
            }
            DisclosureGroup("Hour-by-hour variety") {
                VStack(alignment: .leading, spacing: 12) {
                    ForEach(Array(report.summary.hours.enumerated()), id: \.offset) { _, hour in
                        VStack(alignment: .leading, spacing: 4) {
                            Text("\(DJRehearsal.day(hour.from)) · \(DJRehearsal.clock(hour.from)) · \(DJRehearsal.offset(hour.from))")
                                .font(.subheadline.weight(.medium))
                            Text("\(hour.works) works · mean energy \(hour.energy)" +
                                 (hour.energyTarget.map { " · target \($0)" } ?? ""))
                            Text("Artist variety: \(hour.artistEntropy, specifier: "%.2f") bits · genre variety: \(hour.genreEntropy, specifier: "%.2f") bits")
                                .font(.caption).foregroundStyle(.secondary)
                        }
                    }
                }.padding(.top, 10)
                Text("Variety is the daemon’s entropy measure: zero means one artist or genre; larger values indicate more spread.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            LazyVStack(alignment: .leading, spacing: 16) {
                ForEach(Array(report.works.enumerated()), id: \.offset) { index, pick in
                    work(pick, number: index + 1)
                }
            }
        }
    }

    private func work(_ pick: DJRehearsal.Pick, number: Int) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            workHeading(pick, number: number)
            workDetails(pick)
            workReason(pick.work.reason)
            Divider()
        }
    }

    // Keep each result-builder expression small: this row otherwise exhausts
    // Swift's type checker and prevents the Apple client from building.
    @ViewBuilder private func workHeading(_ pick: DJRehearsal.Pick, number: Int) -> some View {
        Text("\(number). \(pick.work.title)").font(.headline)
        Text(pick.work.composer).font(.subheadline)
        Text("\(DJRehearsal.day(pick.at)) · \(DJRehearsal.clock(pick.at)) · \(DJRehearsal.offset(pick.at))")
            .font(.caption.monospaced()).foregroundStyle(.secondary)
    }

    @ViewBuilder private func workDetails(_ pick: DJRehearsal.Pick) -> some View {
        let length: String = "\(pick.work.movements) movement(s)"
        let minutes: String = pick.work.minutes.map { " · \($0) min" } ?? ""
        let explicit: String = pick.explicit ? " · Explicit" : ""
        let target: String = pick.energyTarget.map { " · target \($0)" } ?? ""
        let genre: String = pick.genre.map { " · \($0)" } ?? ""
        let mood: String = pick.mood.map { " · mood \($0)" } ?? ""
        let energy: String = "Energy \(pick.energy)" + target + genre + mood
        Text(length + minutes + explicit).font(.caption).foregroundStyle(.secondary)
        if let performers = pick.work.performers, !performers.isEmpty {
            Text(performers.joined(separator: ", ")).font(.caption).foregroundStyle(.secondary)
        }
        Text(energy).font(.caption).foregroundStyle(.secondary)
    }

    @ViewBuilder private func workReason(_ reason: DJWork.Reason) -> some View {
        Text(reason.summary).font(.callout).textSelection(.enabled)
        if let relaxed = reason.relaxed, !relaxed.isEmpty {
            Label("Relaxed: " + relaxed.joined(separator: ", "), systemImage: "info.circle")
                .font(.caption)
        }
        if let factors = reason.factors, !factors.isEmpty {
            DisclosureGroup("Why this pick") {
                VStack(alignment: .leading, spacing: 6) {
                    ForEach(factors.indices, id: \.self) { index in
                        factorRow(factors[index])
                    }
                }.padding(.top, 8)
            }
        }
    }

    private func factorRow(_ factor: DJWork.Factor) -> some View {
        let weight = Double(factor.weight) / 1000.0
        return Text("\(factor.factor): \(weight, specifier: "%.3f")×")
            .font(.caption).textSelection(.enabled)
    }

    private func filterFields(_ fields: [DJRehearsalDraft.Filter]) -> some View {
        VStack(alignment: .leading, spacing: 12) {
            ForEach(fields, id: \.self) { field in
                VStack(alignment: .leading, spacing: 4) {
                    Text(field.title).font(.caption).foregroundStyle(.secondary)
                    TextField(field.title, text: Binding(get: { rehearsal.draft.filters[field] ?? "" }, set: { value in
                        var draft = rehearsal.draft; draft.filters[field] = value; rehearsal.update(draft)
                    }), axis: .vertical)
                    .lineLimit(2...5).textFieldStyle(.roundedBorder).accessibilityLabel(field.title)
                }
            }
        }
    }
    private func numberField(_ title: String, text: Binding<String>) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(title).font(.caption).foregroundStyle(.secondary)
            TextField("Optional", text: text).textFieldStyle(.roundedBorder).accessibilityLabel(title)
                #if os(iOS)
                .keyboardType(.numberPad)
                #endif
        }
    }
    private func binding<Value>(_ key: WritableKeyPath<DJRehearsalDraft, Value>) -> Binding<Value> {
        Binding(get: { rehearsal.draft[keyPath: key] }, set: { edit(key, $0) })
    }
    private func edit<Value>(_ key: WritableKeyPath<DJRehearsalDraft, Value>, _ value: Value) {
        guard canEdit else { return }
        var draft = rehearsal.draft; draft[keyPath: key] = value; rehearsal.update(draft)
    }
    private func synchronize() {
        if expanded && remote.active { rehearsal.activate(in: zone) }
        else { rehearsal.suspend() }
    }
}

/// This optional handoff is separate from Generate and export. It builds a
/// local value, then enters the same reviewed writer as the session editor.
@MainActor private struct RehearsalSteeringControls: View {
    @ObservedObject var bridge: RemoteBridge
    let observation: DJRehearsalObservation
    @State private var minutes = ""
    @State private var untilCleared = false
    @State private var problem: String?
    private var remote: RemoteModel { bridge.model }

    var body: some View {
        DisclosureGroup("Use these settings for future DJ picks") {
            VStack(alignment: .leading, spacing: 14) {
                Text("Copy this preview’s explicit mood and filters into a new temporary session for \(observation.request.zone.title) · \(observation.request.zone.household). This replaces all existing temporary settings, not your standing taste preferences.")
                    .font(.callout)
                Text("This does not play the preview, queue its works, or start a stopped DJ. The live session uses the current mood definition and may choose different music. The preview seed, start time and length are not copied.")
                    .font(.caption).foregroundStyle(.secondary)
                if let filters = try? DJSteeringFilters(observation.request.constraints) {
                    Text("Requested mood: \(observation.request.mood ?? "automatic program")\n\(filters.summary)")
                        .font(.callout).textSelection(.enabled)
                }
                Toggle("Keep until explicitly cleared", isOn: $untilCleared)
                    .disabled(!remote.canPrepare)
                if !untilCleared {
                    TextField("New duration in minutes (1–10080)", text: $minutes)
                        .textFieldStyle(.roundedBorder)
                        .accessibilityLabel("Duration of the new temporary DJ settings in minutes")
                        .disabled(!remote.canPrepare)
                        #if os(iOS)
                        .keyboardType(.numberPad)
                        #endif
                }
                Text("Choose a new lifetime starting when saved. The previous session’s deadline is not preserved. Your separate session-editor draft is left untouched.")
                    .font(.caption).foregroundStyle(.secondary)
                if let message = validationMessage {
                    Label(message, systemImage: "info.circle")
                        .font(.caption).foregroundStyle(.secondary)
                }
                Button("Review using these settings", systemImage: "slider.horizontal.3") { review() }
                    .buttonStyle(.borderedProminent).foregroundStyle(RemoteStyle.accentInk)
                    .disabled(!remote.canPrepare || validationMessage != nil)
                    .accessibilityHint("Opens the shared review before replacing temporary steering. Does not start playback.")
                Button("Refresh current DJ and moods", systemImage: "arrow.clockwise") { remote.refreshPanel() }
                    .buttonStyle(.bordered).disabled(!remote.canPrepare || remote.panelLoading)
                if let problem { Text(problem).font(.callout).textSelection(.enabled) }
            }.padding(.top, 12)
        }
        .onChange(of: minutes) { _, _ in problem = nil }
        .onChange(of: untilCleared) { _, _ in problem = nil }
    }

    private var validationMessage: String? {
        do { _ = try proposedChange(); return nil }
        catch { return error.localizedDescription }
    }
    private func proposedChange() throws -> DJSteeringChange {
        guard remote.active, remote.panel == .dj, !remote.panelLoading,
              remote.panelProblem == nil, let zone = remote.selectedZone, let status = remote.dj else {
            throw ContractError.invalid("Refresh the current DJ settings and moods before reviewing a replacement.")
        }
        var draft = try bridge.rehearsal.steeringDraft(from: observation.id, in: zone,
                                                     status: status, moods: remote.moods)
        draft.minutes = minutes; draft.untilCleared = untilCleared
        return try draft.change(moods: remote.moods)
    }
    private func review() {
        guard remote.canPrepare else { return }
        do {
            let change = try proposedChange()
            problem = nil
            remote.prepare(.djFilters(change), in: observation.request.zone)
        } catch { problem = error.localizedDescription }
    }
}
#endif
