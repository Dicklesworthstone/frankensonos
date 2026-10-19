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
    let spotify: SpotifySetupModel
    let endpoint: String
    init(endpoint: DaemonEndpoint) {
        self.endpoint = endpoint.url.absoluteString
        let client = DaemonClient(endpoint: endpoint)
        model = RemoteModel(client: client)
        spotify = SpotifySetupModel(client: client)
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

    var body: some View {
        Group {
            if let bridge {
                RemoteScreen(bridge: bridge) {
                    bridge.spotify.stop(); bridge.model.stop()
                    showSpotify = false
                    self.bridge = nil
                }
                .toolbar { SpotifySetupButton(bridge: bridge, isPresented: $showSpotify) }
                .sheet(isPresented: $showSpotify) { SpotifyPanel(bridge: bridge) }
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
            if phase == .background { bridge?.model.suspend(); bridge?.spotify.suspend() }
            else if phase == .active {
                bridge?.model.resume()
                if showSpotify { bridge?.spotify.resume() }
            }
        }
        .onDisappear { bridge?.spotify.stop(); bridge?.model.stop() }
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
