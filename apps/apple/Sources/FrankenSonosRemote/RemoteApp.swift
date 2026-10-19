import Foundation

#if os(macOS)
import AppKit
import SwiftUI
import FrankenSonosKit

@main @MainActor struct FrankenSonosRemoteApp: App {
    init() { NSApplication.shared.setActivationPolicy(.regular) }
    var body: some SwiftUI.Scene {
        WindowGroup("FrankenSonos") { ConnectionView() }
            .defaultSize(width: 1_120, height: 800)
    }
}

@MainActor final class RemoteBridge: ObservableObject {
    let model: RemoteModel
    let activity: ActivityModel
    let spotify: SpotifySetupModel
    let endpoint: String
    init(endpoint: DaemonEndpoint) {
        self.endpoint = endpoint.url.absoluteString
        let client = DaemonClient(endpoint: endpoint)
        model = RemoteModel(client: client)
        spotify = SpotifySetupModel(client: client)
        activity = ActivityModel(client: client)
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

    var body: some View {
        Group {
            if let bridge {
                RemoteScreen(bridge: bridge) {
                    bridge.spotify.stop(); bridge.activity.stop(); bridge.model.stop()
                    showSpotify = false; showActivity = false; pendingReplay = nil
                    self.bridge = nil
                }
                .toolbar {
                    SpotifySetupButton(bridge: bridge, isPresented: $showSpotify)
                    ActivityButton(bridge: bridge, isPresented: $showActivity)
                }
                .sheet(isPresented: $showSpotify) { SpotifyPanel(bridge: bridge) }
                .sheet(isPresented: $showActivity, onDismiss: { finishReplay(in: bridge) }) {
                    ActivityPanel(bridge: bridge) { selection in
                        pendingReplay = selection; showActivity = false
                    }
                }
            } else {
                VStack(alignment: .leading, spacing: 20) {
                    Label("FrankenSonos", systemImage: "hifispeaker.2.fill").font(.largeTitle.bold())
                    Text("Your house, from anywhere on your tailnet.").font(.title2)
                    Text("Enter the HTTPS origin of your existing FrankenSonos daemon. This remote never connects directly to speakers and never stores Spotify credentials.")
                    TextField("https://your-daemon.your-tailnet.ts.net", text: $origin)
                        .textFieldStyle(.roundedBorder).accessibilityLabel("Daemon origin")
                    Toggle("Allow HTTP to a local host or private LAN address", isOn: $allowHTTP)
                    if let problem { Text(problem).foregroundStyle(.red).textSelection(.enabled) }
                    Button("Connect", action: connect).buttonStyle(.borderedProminent)
                        .disabled(origin.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                    Text("Connecting only reads state. Every playback change is reviewed separately, then sent once.")
                        .font(.callout).foregroundStyle(.secondary)
                }
                .padding(36).frame(maxWidth: 660)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
        .frame(minWidth: 820, minHeight: 620)
        .onChange(of: scenePhase) { _, phase in
            if phase == .background {
                pendingReplay = nil
                bridge?.model.suspend(); bridge?.spotify.suspend(); bridge?.activity.suspend()
            }
            else if phase == .active {
                bridge?.model.resume()
                if showSpotify { bridge?.spotify.resume() }
                if showActivity { bridge?.activity.resume() }
            }
        }
        .onDisappear { pendingReplay = nil; bridge?.spotify.stop(); bridge?.activity.stop(); bridge?.model.stop() }
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
        FileHandle.standardError.write(Data("FrankenSonosRemote requires macOS 14 or later. The portable client and control-session tests run with swift test.\n".utf8))
        exit(1)
    }
}
#endif
