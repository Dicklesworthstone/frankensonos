import SwiftUI

#if os(iOS)
@main
struct FrankenSonosApp: App {
    @Environment(\.scenePhase) private var scenePhase
    @StateObject private var store: MockZoneStore
    @StateObject private var browser = DaemonBrowser()
    init() {
        _store = StateObject(wrappedValue: CommandLine.arguments.contains("-mock") ? MockZoneStore() : MockZoneStore(live: LiveZoneStore()))
    }
    var body: some Scene {
        WindowGroup {
            RemoteRoot()
                .environmentObject(store)
                .onAppear {
                    AppLog.shared.add("app", "launch, daemon \(DaemonSettings.urlString), configured \(DaemonSettings.isConfigured)")
                    store.start()
                    // First launch: nothing chosen yet, so look for a daemon instead of showing "can't reach 127.0.0.1".
                    if store.isLive && !DaemonSettings.isConfigured { browser.start() }
                }
                .onChange(of: browser.found) { _, found in
                    guard !DaemonSettings.isConfigured, let first = found.first else { return }
                    if store.changeDaemonURL(first.url.absoluteString) { browser.stop() }
                }
                .onChange(of: scenePhase) { _, phase in
                    AppLog.shared.add("app", "scene \(phase)")
                    store.setActive(phase == .active)
                }
                .tint(.primary)
                .background(Color(white: 0.025).ignoresSafeArea(edges: .bottom))
                .preferredColorScheme(store.isPlayerPresented ? .dark : nil)
        }
    }
}
#endif
