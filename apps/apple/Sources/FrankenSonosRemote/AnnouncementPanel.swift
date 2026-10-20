#if os(macOS) || os(iOS)
import SwiftUI
import UniformTypeIdentifiers
import FrankenSonosKit

/// No separate network writer: this composer enters RemoteModel's existing
/// one-use review, cancellation, single-flight and persistent-result workflow.
@MainActor struct AnnouncementPanel: View {
    @ObservedObject var bridge: RemoteBridge
    let zone: Zone
    @State private var form: AnnouncementForm
    @State private var problem: String?
    @State private var expanded = false
    @State private var showFilePicker = false
    @State private var pickerID = UUID()
    @State private var fileTask: Task<Void, Never>?
    private var model: RemoteModel { bridge.model }
    private var ready: Bool { model.canPrepare && model.selectedZone?.sameGroup(as: zone) == true }
    private var prepared: Announcement? { try? form.announcement(in: zone) }
    private var speechCount: Int { form.speech.trimmingCharacters(in: .whitespacesAndNewlines).unicodeScalars.count }

    init(bridge: RemoteBridge, zone: Zone) {
        self.bridge = bridge; self.zone = zone
        _form = State(initialValue: AnnouncementForm(group: zone))
    }

    var body: some View {
        let presentedPicker = pickerID
        return DisclosureGroup(isExpanded: $expanded) {
            VStack(alignment: .leading, spacing: 16) {
                Text("Speak a message, play a chime or upload a WAV, then let the daemon restore the music. Choose exactly which rooms in this group should hear it.")
                    .font(.callout).foregroundStyle(.secondary)
                if let notice = form.notice { Text(notice).font(.callout).foregroundStyle(.secondary) }
                VStack(alignment: .leading, spacing: 16) {
                    Picker("Announcement source", selection: $form.kind) {
                        Text("Speech").tag(AnnouncementForm.Kind.speech)
                        Text("Chime").tag(AnnouncementForm.Kind.chime)
                        Text("WAV file").tag(AnnouncementForm.Kind.wav)
                    }.pickerStyle(.segmented)
                    if form.kind == .speech {
                        Text("Message").font(.headline)
                        TextEditor(text: $form.speech)
                            .frame(minHeight: 110, maxHeight: 220)
                            .padding(6)
                            .overlay(RoundedRectangle(cornerRadius: 8).stroke(.secondary.opacity(0.4)))
                            .accessibilityLabel("Announcement message")
                        Text("\(speechCount) / \(Announcement.maximumSpeechScalars) Unicode characters")
                            .font(.caption).foregroundStyle(speechCount > Announcement.maximumSpeechScalars ? Color.red : Color.secondary)
                        Text("Uses the local speech engine and voice configured on the daemon, not this device’s voice.")
                            .font(.caption).foregroundStyle(.secondary)
                    } else if form.kind == .chime {
                        Picker("Chime", selection: $form.chime) {
                            ForEach(Announcement.Chime.allCases, id: \.self) { chime in
                                Text(chime.rawValue.capitalized).tag(chime)
                            }
                        }
                    } else { fileSource }
                    Divider()
                    Text("Recipients · \(zone.household)").font(.headline)
                    ForEach(zone.members, id: \.self) { room in
                        Toggle(room, isOn: Binding(get: { form.selectedRooms.contains(room) }, set: {
                            form.select(room, selected: $0); problem = nil
                        }))
                        .accessibilityLabel("Announce in \(room), household \(zone.household)")
                        .frame(minHeight: 44)
                    }
                    ViewThatFits(in: .horizontal) {
                        HStack { selectionButtons }
                        VStack(alignment: .leading) { selectionButtons }
                    }
                    Stepper("Requested volume: \(form.volume)", value: $form.volume, in: 0...100)
                        .accessibilityLabel("Announcement volume")
                        .accessibilityValue(String(form.volume))
                    Text("House limits can lower this level. The result reports actual announcement levels and anything the daemon could not restore.")
                        .font(.caption).foregroundStyle(.secondary)
                    if prepared == nil {
                        Text(form.selectedRooms.isEmpty ? "Choose at least one room. No rooms are selected by default."
                             : form.kind == .wav ? "Choose a valid WAV and finish its local read before reviewing."
                             : "Enter a nonempty message of at most 1000 Unicode characters, or choose a chime.")
                            .font(.callout).foregroundStyle(.secondary)
                    }
                    if let problem { Text(problem).font(.callout).foregroundStyle(.secondary).textSelection(.enabled) }
                    Button {
                        do {
                            let announcement = try form.announcement(in: zone)
                            problem = nil
                            model.prepare(.announce(announcement), in: zone)
                        } catch { problem = error.localizedDescription }
                    } label: {
                        Label("Review announcement", systemImage: "megaphone")
                            .frame(maxWidth: .infinity, minHeight: 32)
                    }
                    .buttonStyle(.borderedProminent).foregroundStyle(RemoteStyle.accentInk)
                    .disabled(prepared == nil || showFilePicker)
                    .accessibilityHint("Shows the exact message or file details, recipients and volume before sending. Nothing plays yet.")
                }.disabled(!ready)
                Text("Announcements interrupt playback temporarily and are not undoable. Closing the remote cannot cancel a sent clip; check the result before repeating it.")
                    .font(.caption).foregroundStyle(.secondary)
            }.padding(.top, 16)
        } label: {
            Label("Announcements", systemImage: "megaphone.fill").font(.headline).frame(minHeight: 44)
        }
        .remoteCard()
        .fileImporter(isPresented: $showFilePicker, allowedContentTypes: [.wav]) { result in
            receiveFile(result, picker: presentedPicker)
        }
        .onChange(of: zone) { _, current in
            if !form.group.sameGroup(as: current) { cancelFileRead() }
            form.observe(current); problem = nil
        }
        .onChange(of: form.kind) { _, _ in cancelFileRead(); problem = nil }
        .onChange(of: expanded) { _, visible in if !visible { cancelFileRead() } }
        .onChange(of: model.active) { _, active in if !active { cancelFileRead() } }
        .onDisappear { cancelFileRead() }
    }

    private var fileSource: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Choose a mono or stereo 16-bit PCM WAV, up to 16 MiB and five minutes. It is read locally now and uploaded only after confirmation.")
                .font(.callout).foregroundStyle(.secondary)
            if let clip = form.wav {
                Text(clip.summary).font(.callout).textSelection(.enabled)
                Text(String(format: "Length: %.2f seconds", clip.duration)).font(.caption).foregroundStyle(.secondary)
            }
            if form.fileLoading {
                ProgressView("Reading and validating the WAV…")
                Button("Cancel file read") { cancelFileRead() }.buttonStyle(.bordered)
            } else {
                Button(form.wav == nil ? "Choose WAV" : "Replace WAV", systemImage: "doc.badge.plus") {
                    pickerID = UUID(); problem = nil; showFilePicker = true
                }.buttonStyle(.bordered)
                if form.wav != nil {
                    Button("Remove selected file") { cancelFileRead(); form.removeFile() }.buttonStyle(.bordered)
                }
            }
            Text("The file is not previewed or played during selection. The review retains the loaded bytes, not a path on either device.")
                .font(.caption).foregroundStyle(.secondary)
        }
    }

    private func receiveFile(_ result: Result<URL, Error>, picker: UUID) {
        guard pickerID == picker, ready, expanded, form.kind == .wav else { return }
        switch result {
        case .failure:
            form.removeFile()
            problem = "The file picker could not open the WAV. Choose it again. Nothing was uploaded."
        case .success(let url):
            fileTask?.cancel()
            let id = form.beginFileImport()
            fileTask = Task {
                do {
                    let clip = try await AnnouncementWav.load(from: url)
                    guard !Task.isCancelled, ready, expanded else {
                        _ = form.failFileImport(id: id); return
                    }
                    if form.finishFileImport(clip, id: id) { problem = nil }
                } catch {
                    if form.failFileImport(id: id), !Task.isCancelled { problem = error.localizedDescription }
                }
            }
        }
    }

    private func cancelFileRead() {
        pickerID = UUID(); showFilePicker = false
        fileTask?.cancel(); fileTask = nil
        form.cancelFileImport()
    }

    @ViewBuilder private var selectionButtons: some View {
        Button("Select this group’s rooms") { form.selectAll(); problem = nil }.buttonStyle(.bordered)
        Button("Clear selection") { form.clearRooms(); problem = nil }.buttonStyle(.bordered)
    }
}
#endif
