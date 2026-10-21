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
                    bridge.spotify.stop(); bridge.activity.stop(); bridge.catalog.stop(); bridge.rehearsal.stop(); bridge.model.stop()
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
                bridge?.model.suspend(); bridge?.spotify.suspend(); bridge?.activity.suspend(); bridge?.catalog.suspend(); bridge?.rehearsal.suspend()
            }
            else if phase == .active {
                bridge?.model.resume()
                if showSpotify { bridge?.spotify.resume() }
                if showActivity { bridge?.activity.resume() }
            }
        }
        .onDisappear {
            pendingReplay = nil
            bridge?.spotify.stop(); bridge?.activity.stop(); bridge?.catalog.stop(); bridge?.rehearsal.stop(); bridge?.model.stop()
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
#else
@main struct UnsupportedRemote {
    static func main() {
        FileHandle.standardError.write(Data("FrankenSonosRemote requires iOS 17 or macOS 14 or later. The portable client and control-session tests run with swift test.\n".utf8))
        exit(1)
    }
}
#endif
