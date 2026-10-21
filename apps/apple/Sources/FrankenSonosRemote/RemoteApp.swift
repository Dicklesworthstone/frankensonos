import Foundation

#if os(macOS) || os(iOS)
import SwiftUI
import FrankenSonosKit
#if os(macOS)
import AppKit
#endif

@main @MainActor struct FrankenSonosRemoteApp: App {
    #if os(macOS)
    init() { NSApplication.shared.setActivationPolicy(.regular) }
    #endif
    var body: some SwiftUI.Scene {
        WindowGroup("FrankenSonos") { ConnectionView() }
            #if os(macOS)
            .defaultSize(width: 1_120, height: 800)
            #endif
    }
}

@MainActor final class RemoteBridge: ObservableObject {
    let model: RemoteModel
    let activity: ActivityModel
    let spotify: SpotifySetupModel
    let catalog: LibraryCatalogModel
    let rehearsal: DJRehearsalModel
    let diagnostics: DiagnosticsModel
    let settingsReader: ListeningSettingsModel.Reader
    let endpoint: String
    init(endpoint: DaemonEndpoint) {
        self.endpoint = endpoint.url.absoluteString
        let client = DaemonClient(endpoint: endpoint)
        settingsReader = { try await client.listeningSettings($0) }
        model = RemoteModel(client: client)
        spotify = SpotifySetupModel(client: client)
        activity = ActivityModel(client: client)
        catalog = LibraryCatalogModel(client: client)
        rehearsal = DJRehearsalModel(read: { try await client.rehearse($0) })
        diagnostics = DiagnosticsModel(client: client)
        diagnostics.onChange = { [weak self] in self?.objectWillChange.send() }
        rehearsal.onChange = { [weak self] in self?.objectWillChange.send() }
        catalog.onChange = { [weak self] in self?.objectWillChange.send() }
        activity.onChange = { [weak self] in self?.objectWillChange.send() }
        model.onChange = { [weak self] in self?.objectWillChange.send() }
        spotify.onChange = { [weak self] in self?.objectWillChange.send() }
    }
}

@MainActor struct ConnectionView: View {
    @Environment(\.scenePhase) private var scenePhase
    @AppStorage("frankensonos.daemonOrigin") private var origin = ""
    @AppStorage("frankensonos.localHTTP") private var allowHTTP = false
    @State private var bridge: RemoteBridge?
    @State private var problem: String?
    @State private var showSpotify = false
    @State private var showActivity = false
    @State private var pendingReplay: ActivityReplaySelection?
    @State private var replayProblem: String?
    @State private var restoredConnection = false
    @State private var showLocalOptions = false

    var body: some View {
        Group {
            if let bridge {
                RemoteScreen(bridge: bridge, openSpotify: { showSpotify = true }, openActivity: { showActivity = true }) {
                    bridge.spotify.stop(); bridge.activity.stop(); bridge.catalog.stop(); bridge.rehearsal.stop(); bridge.diagnostics.stop(); bridge.model.stop()
                    showSpotify = false; showActivity = false; pendingReplay = nil
                    self.bridge = nil
                }
                .sheet(isPresented: $showSpotify) { SpotifyPanel(bridge: bridge) }
                .sheet(isPresented: $showActivity, onDismiss: { finishReplay(in: bridge) }) {
                    ActivityPanel(bridge: bridge) { selection in
                        pendingReplay = selection; showActivity = false
                    }
                }
            } else {
                ScrollView {
                    VStack(alignment: .leading, spacing: 28) {
                        VStack(alignment: .leading, spacing: 18) {
                            Image(systemName: "hifispeaker.2.fill")
                                .font(.system(size: 38, weight: .medium))
                                .foregroundStyle(RemoteStyle.accentInk)
                                .frame(width: 88, height: 88)
                                .background(RemoteStyle.accent.gradient, in: RoundedRectangle(cornerRadius: 26))
                                .accessibilityHidden(true)
                            Text("FrankenSonos").font(.headline).foregroundStyle(.secondary)
                            Text("Your music.\nEvery room.")
                                .font(.largeTitle.bold()).fixedSize(horizontal: false, vertical: true)
                            Text("Command your speakers from anywhere on your Tailscale tailnet.")
                                .font(.title3).foregroundStyle(.secondary)
                        }
                        VStack(alignment: .leading, spacing: 16) {
                            Text("Connect to your house").font(.title2.bold())
                            Text("Enter the HTTPS address of your FrankenSonos daemon.")
                                .foregroundStyle(.secondary)
                            TextField("https://your-daemon.your-tailnet.ts.net", text: $origin)
                                .textFieldStyle(.roundedBorder)
                                .accessibilityLabel("Daemon address")
                                #if os(iOS)
                                .keyboardType(.URL)
                                .textInputAutocapitalization(.never)
                                .autocorrectionDisabled()
                                .submitLabel(.go)
                                #endif
                                .onSubmit(connect)
                            if let problem {
                                Label(problem, systemImage: "exclamationmark.circle")
                                    .foregroundStyle(.red).textSelection(.enabled)
                                    .fixedSize(horizontal: false, vertical: true)
                            }
                            Button(action: connect) {
                                Label("Connect", systemImage: "arrow.right")
                                    .frame(maxWidth: .infinity, minHeight: 36)
                            }
                            .buttonStyle(.borderedProminent).foregroundStyle(RemoteStyle.accentInk)
                            .disabled(origin.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                            DisclosureGroup("Local connection options", isExpanded: $showLocalOptions) {
                                Toggle("Allow HTTP to a local host or private LAN address", isOn: $allowHTTP)
                                    .padding(.top, 8)
                            }.font(.callout)
                        }
                        .remoteCard()
                        Label("Your speakers stay on your LAN. This app connects through your daemon.", systemImage: "network")
                            .font(.callout).foregroundStyle(.secondary)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                    .padding(24).padding(.vertical, 28)
                    .frame(maxWidth: 600)
                    .frame(maxWidth: .infinity)
                }
            }
        }
        .tint(RemoteStyle.accent)
        .background(RemoteStyle.background)
        #if os(macOS)
        .frame(minWidth: 700, minHeight: 560)
        #endif
        .onAppear {
            guard !restoredConnection else { return }
            restoredConnection = true
            if !origin.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty { connect() }
        }
        .onChange(of: scenePhase) { _, phase in
            if phase == .background {
                pendingReplay = nil
                bridge?.model.suspend(); bridge?.spotify.suspend(); bridge?.activity.suspend(); bridge?.catalog.suspend(); bridge?.rehearsal.suspend(); bridge?.diagnostics.suspend()
            }
            else if phase == .active {
                bridge?.model.resume()
                if showSpotify { bridge?.spotify.resume() }
                if showActivity { bridge?.activity.resume() }
            }
        }
        .onDisappear {
            pendingReplay = nil
            bridge?.spotify.stop(); bridge?.activity.stop(); bridge?.catalog.stop(); bridge?.rehearsal.stop(); bridge?.diagnostics.stop(); bridge?.model.stop()
        }
        .alert("Replay was not started", isPresented: Binding(get: { replayProblem != nil }, set: { if !$0 { replayProblem = nil } })) {
            Button("OK") { replayProblem = nil }
        } message: { Text(replayProblem ?? "") }
    }

    private func finishReplay(in bridge: RemoteBridge) {
        guard let selected = pendingReplay else { return }
        pendingReplay = nil
        do {
            guard bridge.model.canSchedule else {
                throw ContractError.invalid("The remote is suspended or another command is active. Review the source again.")
            }
            _ = try selected.validated(against: bridge.model.house?.snapshot?.zones ?? [])
            // Wait for Activity dismissal before opening the existing review
            // sheet. Selection is explicit and fresh command reads still run.
            bridge.model.select(selected.zone.id)
            bridge.model.prepare(selected.command, in: selected.zone)
        } catch { replayProblem = error.localizedDescription }
    }

    private func connect() {
        do {
            let endpoint = try DaemonEndpoint(origin, allowLocalHTTP: allowHTTP)
            origin = endpoint.url.absoluteString
            let session = RemoteBridge(endpoint: endpoint)
            bridge = session; problem = nil
            session.model.resume()
        } catch { problem = error.localizedDescription }
    }
}

@MainActor struct DiagnosticsPanel: View {
    @Environment(\.dismiss) private var dismiss
    @Environment(\.scenePhase) private var scenePhase
    @ObservedObject var bridge: RemoteBridge
    @State private var filter: DiagnosticsReport.Filter = .attention
    @State private var query = ""
    @State private var includeEvidence = false
    private var model: DiagnosticsModel { bridge.diagnostics }

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 20) {
                    introduction
                    if let problem = model.problem {
                        Label(problem, systemImage: "exclamationmark.circle")
                            .font(.callout).textSelection(.enabled).remoteCard()
                    }
                    if let observation = model.observation {
                        report(observation)
                    } else if !model.loading && model.problem == nil {
                        Text("No diagnostic report has been requested. Run checks even when no rooms have been discovered.")
                            .font(.callout).foregroundStyle(.secondary)
                    }
                }
                .padding(20).frame(maxWidth: 900, alignment: .leading)
                .frame(maxWidth: .infinity)
            }
            .background(RemoteStyle.background)
            .navigationTitle("Setup diagnostics")
            #if os(iOS)
            .navigationBarTitleDisplayMode(.inline)
            #endif
            .toolbar {
                ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } }
            }
        }
        #if os(macOS)
        .frame(minWidth: 600, idealWidth: 780, minHeight: 520, idealHeight: 720)
        #endif
        .presentationDetents([.large])
        .onAppear { if scenePhase == .active && bridge.model.active { model.resume() } }
        .onDisappear { model.suspend() }
        .onChange(of: scenePhase) { _, phase in
            if phase == .background { model.suspend() }
            else if phase == .active && bridge.model.active { model.resume() }
        }
        .onChange(of: bridge.model.active) { _, active in
            if !active { model.suspend() }
            else if scenePhase == .active { model.resume() }
        }
    }

    private var introduction: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Find what is stopping your house").font(.title2.bold()).accessibilityAddTraits(.isHeader)
            Text("Ask your daemon to check its setup and show the remedies it recommends. This does not play music, change groups or levels, sign in to Spotify, or execute repairs.")
                .font(.callout).foregroundStyle(.secondary)
            if model.loading {
                HStack(spacing: 12) {
                    ProgressView().controlSize(.small)
                    Text("Waiting for the daemon’s checks…").font(.callout)
                }
                Button("Stop waiting", systemImage: "xmark.circle") { model.cancel() }
                    .buttonStyle(.bordered)
                Text("Stopping or closing this screen cancels the client read. The daemon may still finish its checks.")
                    .font(.caption).foregroundStyle(.secondary)
            } else {
                Button("Run setup checks", systemImage: "stethoscope") { model.run() }
                    .buttonStyle(.borderedProminent).foregroundStyle(RemoteStyle.accentInk)
                    .disabled(!model.canRun || !bridge.model.active)
                    .accessibilityHint("Requests one read-only report from the configured daemon. No room selection is required.")
            }
            Text("A failed connection is different from a failed check. If the daemon cannot be reached, this remote cannot diagnose its host or the speaker LAN.")
                .font(.caption).foregroundStyle(.secondary)
        }.remoteCard()
    }

    private func report(_ observation: DiagnosticsObservation) -> some View {
        let report = observation.report
        let visible = report.filtered(filter, query: query)
        return VStack(alignment: .leading, spacing: 16) {
            VStack(alignment: .leading, spacing: 8) {
                if !model.isCurrent {
                    Label("Previous report — not a current check", systemImage: "clock.arrow.circlepath")
                        .font(.subheadline.weight(.semibold))
                }
                Text(report.headline).font(.title3.bold())
                Text(report.summary).font(.callout)
                Text("Received \(observation.receivedAt.formatted(date: .abbreviated, time: .standard))")
                    .font(.caption).foregroundStyle(.secondary)
                Text("The daemon may reuse a recent report. Skipped checks were not verified; a passing check is not proof that all playback works.")
                    .font(.caption).foregroundStyle(.secondary)
            }.remoteCard()
            Picker("Which checks", selection: $filter) {
                ForEach(DiagnosticsReport.Filter.allCases, id: \.self) { item in
                    Text(item.title).tag(item)
                }
            }.pickerStyle(.menu)
            TextField("Find a check or remedy", text: $query)
                .textFieldStyle(.roundedBorder).accessibilityLabel("Filter diagnostic checks")
            if visible.isEmpty {
                Text(report.checks.isEmpty ? "This response contains no checks." : "No checks match this filter. Choose All checks to inspect the complete report.")
                    .font(.callout).foregroundStyle(.secondary)
            }
            LazyVStack(alignment: .leading, spacing: 12) {
                ForEach(visible) { check in checkCard(check) }
            }
            DisclosureGroup("Share diagnostic text") {
                VStack(alignment: .leading, spacing: 12) {
                    Text("Review before sharing: summaries, remedies and evidence can contain private room names, network addresses and local paths. Nothing is uploaded automatically.")
                        .font(.callout).foregroundStyle(.secondary)
                    Toggle("Include machine evidence", isOn: $includeEvidence)
                    Text("The full report is shared, not just the filtered checks.")
                        .font(.caption).foregroundStyle(.secondary)
                    ShareLink(item: export(observation)) {
                        Label("Share report as text", systemImage: "square.and.arrow.up")
                    }.buttonStyle(.bordered).disabled(!model.active)
                }.padding(.top, 12)
            }.remoteCard()
        }
    }

    private func checkCard(_ check: DiagnosticsReport.Check) -> some View {
        VStack(alignment: .leading, spacing: 9) {
            Label(check.status.title + " · " + check.title, systemImage: symbol(check.status))
                .font(.headline)
            Text(check.id).font(.caption.monospaced()).foregroundStyle(.secondary).textSelection(.enabled)
            Text(check.summary).font(.callout).textSelection(.enabled)
            if let detail = check.detail, !detail.isEmpty {
                Text(detail).font(.callout).foregroundStyle(.secondary).textSelection(.enabled)
            }
            if let remedy = check.remedy, !remedy.isEmpty {
                Text("Suggested remedy").font(.subheadline.bold())
                Text(remedy).font(.callout).textSelection(.enabled)
            }
            if check.status == .skip {
                Text("Not verified by this run.").font(.caption).foregroundStyle(.secondary)
            } else {
                Text("Check duration: \(check.durationMs) ms").font(.caption).foregroundStyle(.secondary)
            }
            if let evidence = check.evidenceText {
                DisclosureGroup("Machine evidence") {
                    Text(evidence).font(.caption.monospaced()).textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading).padding(.top, 8)
                }.font(.caption)
            }
        }.frame(maxWidth: .infinity, alignment: .leading).remoteCard()
    }
    private func symbol(_ status: DiagnosticsReport.Status) -> String {
        switch status {
        case .pass: "checkmark.circle"
        case .warn: "exclamationmark.triangle"
        case .fail: "xmark.octagon"
        case .skip: "minus.circle"
        }
    }
    private func export(_ observation: DiagnosticsObservation) -> String {
        let date = ISO8601DateFormatter().string(from: observation.receivedAt)
        return (model.isCurrent ? "Latest received report" : "Previous report — not current")
            + "\nReceived: \(date)\n\n" + observation.report.text(includeEvidence: includeEvidence)
    }
}

#else
@main struct UnsupportedRemote {
    static func main() {
        FileHandle.standardError.write(Data("FrankenSonosRemote requires iOS 17 or macOS 14 or later. The portable client and control-session tests run with swift test.\n".utf8))
        exit(1)
    }
}
#endif
