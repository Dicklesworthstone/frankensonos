#if os(macOS) || os(iOS)
import SwiftUI
import FrankenSonosKit

@MainActor struct SchedulesPanel: View {
    @ObservedObject var bridge: RemoteBridge
    @State private var suggestedDate = Date().addingTimeInterval(3_600)
    private var model: RemoteModel { bridge.model }
    private var rooms: [RoomID] {
        Array(Set(model.house?.snapshot?.zones.flatMap { Array($0.rooms) } ?? []))
            .sorted { $0.address < $1.address }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            Text("Music that fits your day. Schedules keep running when you close the app; weekly times follow your house’s time zone.")
                .font(.callout).foregroundStyle(.secondary)
            if let observed = model.schedulesObservedAt {
                Text("Schedule list observed: \(observed.formatted(date: .omitted, time: .standard))")
                    .font(.caption).foregroundStyle(.secondary)
                if model.schedules.isEmpty {
                    EmptyMusicState(title: "Make music part of your routine", systemImage: "calendar",
                                    detail: "Create a schedule below to start your DJ, pause music, adjust a room’s volume or apply a saved scene.")
                }
            } else if !model.panelLoading {
                Text("Schedules are not available. Refresh before creating or changing one.")
            }
            ForEach(model.schedules) { schedule in
                GroupBox {
                    VStack(alignment: .leading, spacing: 8) {
                        HStack {
                            Text("#\(schedule.id) • \(schedule.when)").font(.headline)
                            Spacer()
                            Text(schedule.stateLabel).font(.caption)
                        }
                        Text(schedule.summary)
                        if let direction = schedule.djSummary { Text(direction).textSelection(.enabled) }
                        Text(schedule.targetLabel).textSelection(.enabled)
                        Text("Runs as: \(schedule.creator)").font(.caption).textSelection(.enabled)
                        if let next = schedule.next { Text("Next run: \(next)").font(.caption).monospacedDigit() }
                        if let last = schedule.lastFired { Text("Last processed run: \(last) (may have been skipped)").font(.caption) }
                        ViewThatFits(in: .horizontal) {
                            HStack {
                                Button(schedule.enabled ? "Pause schedule" : "Resume schedule") {
                                    model.prepareSchedule(.change(schedule, schedule.enabled ? .pause : .resume))
                                }.buttonStyle(.bordered)
                                scheduleMenu(schedule)
                            }
                            VStack(alignment: .leading) {
                                Button(schedule.enabled ? "Pause schedule" : "Resume schedule") {
                                    model.prepareSchedule(.change(schedule, schedule.enabled ? .pause : .resume))
                                }.buttonStyle(.bordered)
                                scheduleMenu(schedule)
                            }
                        }.disabled(!model.canManageSchedules)
                    }.frame(maxWidth: .infinity, alignment: .leading)
                }
            }
            GroupBox("Create a schedule") {
                VStack(alignment: .leading, spacing: 14) {
                    timeFields
                    actionFields
                    if let problem = model.scheduleFormProblem { Text(problem).foregroundStyle(.red).textSelection(.enabled) }
                    Text("Review checks the target before storing one definition. Creation does not play music now. The daemon checks policy and computes its next run.")
                        .font(.caption).foregroundStyle(.secondary)
                    Button("Review new schedule") { model.prepareScheduleDraft() }
                        .buttonStyle(.borderedProminent).foregroundStyle(RemoteStyle.accentInk)
                        .disabled(!model.canManageSchedules)
                }.padding(6)
            }
        }
    }

    private var timeFields: some View {
        VStack(alignment: .leading, spacing: 10) {
            Picker("When", selection: field(\.frequency)) {
                Text("Weekly / recurring").tag(ScheduleForm.Frequency.weekly)
                Text("Once, at an explicit time").tag(ScheduleForm.Frequency.once)
            }.pickerStyle(.menu)
            if model.scheduleForm.frequency == .weekly {
                LazyVGrid(columns: [GridItem(.adaptive(minimum: 66), spacing: 8)], spacing: 8) {
                    ForEach(ScheduleDay.allCases, id: \.self) { day in
                        Toggle(day.rawValue.capitalized, isOn: Binding(get: { model.scheduleForm.days.contains(day) }, set: { enabled in
                            var form = model.scheduleForm
                            if enabled { form.days.insert(day) } else { form.days.remove(day) }
                            model.updateScheduleForm(form)
                        })).toggleStyle(.button).frame(minHeight: 44)
                    }
                }
                VStack(alignment: .leading, spacing: 8) {
                    Text("Time at your house").font(.subheadline.weight(.medium))
                    TextField("HH:MM", text: field(\.time)).textFieldStyle(.roundedBorder).frame(width: 100)
                        .accessibilityLabel("Schedule house-local time, HH:MM")
                        #if os(iOS)
                        .keyboardType(.numbersAndPunctuation)
                        #endif
                }
                Text("Use 24-hour time in your house’s time zone, even when you’re away. Daylight-saving changes are handled by the daemon.")
                    .font(.caption).foregroundStyle(.secondary)
            } else {
                DatePicker("Date and time", selection: onceDate, displayedComponents: [.date, .hourAndMinute])
                    .datePickerStyle(.compact)
                Text("Shown in this device’s time zone and saved as an exact instant.")
                    .font(.caption).foregroundStyle(.secondary)
                DisclosureGroup("Exact time and offset") {
                    TextField("2028-03-12T07:30:00-04:00", text: field(\.once)).textFieldStyle(.roundedBorder)
                        .accessibilityLabel("One-time date with seconds and UTC offset")
                        #if os(iOS)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        #endif
                }.font(.caption)
            }
        }
        .disabled(!model.canSchedule)
        .onChange(of: model.scheduleForm.frequency) { _, frequency in
            if frequency == .once && model.scheduleForm.once.isEmpty { onceDate.wrappedValue = suggestedDate }
        }
    }

    private var actionFields: some View {
        VStack(alignment: .leading, spacing: 10) {
            Picker("Action", selection: field(\.action)) {
                Text("Start DJ").tag(ScheduleForm.Action.dj)
                Text("Pause music").tag(ScheduleForm.Action.pause)
                Text("Set room volume").tag(ScheduleForm.Action.volume)
                Text("Apply house scene").tag(ScheduleForm.Action.scene)
            }.pickerStyle(.menu)
            if model.scheduleForm.action == .scene {
                if let problem = model.scheduleScenesProblem { Text(problem).font(.callout).textSelection(.enabled) }
                Picker("Saved scene", selection: field(\.scene)) {
                    Text("Choose a saved scene").tag("")
                    if !model.scheduleForm.scene.isEmpty && !model.scheduleScenes.contains(where: { $0.name == model.scheduleForm.scene }) {
                        Text("Unavailable: \(model.scheduleForm.scene)").tag(model.scheduleForm.scene)
                    }
                    ForEach(model.scheduleScenes) { scene in Text(scene.name).tag(scene.name) }
                }
                Text("Each run applies the definition saved under that name at run time, including rooms outside the selected household.")
                    .font(.caption).foregroundStyle(.secondary)
            } else {
                Picker("Exact scheduled room", selection: field(\.room)) {
                    Text("Choose a room and household").tag(Optional<RoomID>.none)
                    if let chosen = model.scheduleForm.room, !rooms.contains(chosen) {
                        Text("Unavailable: \(chosen.address)").tag(Optional(chosen))
                    }
                    ForEach(rooms) { room in Text(room.address).tag(Optional(room)) }
                }
                if let selected = model.selectedRoom {
                    Button("Use selected room: \(selected.address)") {
                        var form = model.scheduleForm; form.room = selected; model.updateScheduleForm(form)
                    }
                }
                switch model.scheduleForm.action {
                case .dj:
                    TextField("Optional DJ mood (blank uses the program)", text: field(\.mood)).textFieldStyle(.roundedBorder)
                case .pause:
                    TextField("Fade seconds, 0–600", text: field(\.fadeSeconds)).textFieldStyle(.roundedBorder)
                        .accessibilityLabel("Scheduled pause fade seconds, zero to six hundred")
                case .volume:
                    TextField("Volume, 0–100", text: field(\.volume)).textFieldStyle(.roundedBorder)
                        .accessibilityLabel("Scheduled room volume, zero to one hundred")
                case .scene: EmptyView()
                }
                Text("The named room stays fixed in this draft. Playback grouping is resolved by the daemon when the schedule runs; volume targets this room.")
                    .font(.caption).foregroundStyle(.secondary)
            }
        }.disabled(!model.canSchedule)
    }

    private func field<Value>(_ key: WritableKeyPath<ScheduleForm, Value>) -> Binding<Value> {
        Binding(get: { model.scheduleForm[keyPath: key] }, set: { value in
            var form = model.scheduleForm; form[keyPath: key] = value; model.updateScheduleForm(form)
        })
    }

    private var onceDate: Binding<Date> {
        Binding(get: {
            ISO8601DateFormatter().date(from: model.scheduleForm.once) ?? suggestedDate
        }, set: { value in
            let minute = Date(timeIntervalSince1970: (value.timeIntervalSince1970 / 60).rounded(.down) * 60)
            suggestedDate = minute
            var form = model.scheduleForm
            form.once = ISO8601DateFormatter().string(from: minute)
            model.updateScheduleForm(form)
        })
    }

    private func scheduleMenu(_ schedule: SavedSchedule) -> some View {
        Menu {
            Button("Remove schedule", systemImage: "trash", role: .destructive) {
                model.prepareSchedule(.change(schedule, .remove))
            }
        } label: { Label("More", systemImage: "ellipsis").frame(minHeight: 32) }
            .buttonStyle(.bordered)
    }
}

@MainActor struct ScheduleReviewSheet: View {
    let review: ScheduleReview
    let confirm: () -> Void
    let cancel: () -> Void
    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 16) {
                    Text(review.command.title).font(.title.bold()).accessibilityAddTraits(.isHeader)
                    Text(review.command.detail).textSelection(.enabled)
                    if let current = review.current {
                        Text("Current state: \(current.stateLabel)")
                        if let next = current.next { Text("Daemon-reported next run: \(next)") }
                        if let last = current.lastFired { Text("Last processed: \(last) — not proof of successful playback") }
                    }
                    if case .add(let draft) = review.command {
                        switch draft.time {
                        case .weekly: Text("This weekly time belongs to the house’s time zone, even when this device is elsewhere.")
                        case .once: Text("This is a single instant with the explicit offset shown above.")
                        }
                        if let scene = draft.action.savedScene {
                            SceneDetails(scene: scene)
                            Text("The schedule stores this scene’s name. Future runs use the definition saved under that name then.")
                        }
                        Text("The daemon assigns your caller identity as creator. Each run uses that creator’s permissions and the house’s current policy.")
                    }
                    Text("Pausing prevents future runs; resuming skips missed runs. Removing the definition does not stop a run already executing. No music is started by this confirmation itself.")
                        .font(.callout).foregroundStyle(.secondary)
                    Divider()
                    Text("The stored definition or target is checked again before one request. These checks are not a reservation. An uncertain reply is never retried.")
                        .font(.caption).foregroundStyle(.secondary)
                }.padding(24).frame(maxWidth: .infinity, alignment: .leading)
            }
            .navigationTitle("Review schedule")
            #if os(iOS)
            .navigationBarTitleDisplayMode(.inline)
            #endif
            .safeAreaInset(edge: .bottom) { ReviewActions(confirm: confirm, cancel: cancel) }
        }.frame(maxWidth: 560)
        #if os(macOS)
        .frame(width: 560, height: 600)
        #endif
        .presentationDetents([.large])
        .presentationDragIndicator(.visible)
        .interactiveDismissDisabled()
    }
}
#endif
