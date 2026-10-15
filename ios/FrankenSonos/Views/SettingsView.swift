import SwiftUI
#if canImport(UIKit)
import UIKit
#endif

struct SettingsView: View {
    @EnvironmentObject private var store: MockZoneStore
    @Environment(\.colorScheme) private var scheme
    @State private var daemonURL = DaemonSettings.urlString
    @State private var invalidURL = false
    @State private var detail: String?
    @StateObject private var browser = DaemonBrowser()
    @State private var copiedLines: Int?
    private let sections: [(String, [(String, String, String)])] = [
        ("System", [("house", "My System", "9 speakers"), ("slider.horizontal.3", "Room Settings", ""), ("alarm", "Alarms", "None set")]),
        ("Services and Voice", [("music.note", "Music Services", "3 services"), ("mic", "Voice Assistants", "Not set up")]),
        ("Account", [("person.crop.circle", "Your Account", "Demo account")]),
        ("Help", [("questionmark.circle", "Help & Tips", ""), ("info.circle", "About My System", "FrankenSonos")])
    ]
    private var daemonSettings: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("DAEMON").s1Font(11, weight: .semibold).foregroundStyle(.secondary)
            TextField("Daemon URL", text: $daemonURL).textFieldStyle(.plain).s1Font(14)
                .onSubmit(saveURL).accessibilityLabel("Daemon URL")
                #if os(iOS)
                .keyboardType(.URL).textInputAutocapitalization(.never).autocorrectionDisabled()
                #endif
            HStack {
                Text("Connection").s1Font(14)
                Spacer()
                Text(store.connectionStatus.rawValue.capitalized).s1Font(12).foregroundStyle(.secondary)
            }
            HStack {
                Text(URL(string: store.daemonURLString)?.host ?? store.daemonURLString).s1Font(11).foregroundStyle(.secondary)
                Spacer()
                // Connect only when the field holds an address other than the one in use; Retry when that one is down.
                if !addressIsCurrent {
                    Button("Connect", action: saveURL).s1Font(13, weight: .semibold)
                } else if store.connectionStatus == .offline {
                    Button("Retry", action: store.retry).s1Font(13, weight: .semibold)
                }
            }
            if invalidURL { Text("Enter an http:// or https:// daemon URL.").s1Font(12).foregroundStyle(.red) }
            foundDaemons
        }
        .padding(16).background(S1Palette.panel(scheme))
        .onAppear { browser.start() }
        .onDisappear { browser.stop() }
    }
    private var addressIsCurrent: Bool {
        DaemonSettings.validated(daemonURL)?.absoluteString == store.daemonURLString
    }
    /// Daemons advertising themselves on this network; tapping one connects to it.
    @ViewBuilder private var foundDaemons: some View {
        Text("FOUND ON YOUR NETWORK").s1Font(11, weight: .semibold).foregroundStyle(.secondary).padding(.top, 4)
        if browser.found.isEmpty {
            Text("Looking for daemons. Away from home, type the address above.").s1Font(12).foregroundStyle(.secondary)
        }
        ForEach(browser.found) { daemon in
            Button {
                daemonURL = daemon.url.absoluteString
                saveURL()
            } label: {
                HStack {
                    Text(daemon.name).s1Font(14)
                    Spacer()
                    Text(daemon.url.host ?? "").s1Font(11).foregroundStyle(.secondary)
                    if daemon.url.absoluteString == store.daemonURLString {
                        Image(systemName: "checkmark").font(.system(size: 12, weight: .semibold))
                    }
                }
            }
            .buttonStyle(.plain)
        }
    }
    private func saveURL() { invalidURL = !store.changeDaemonURL(daemonURL) }
    /// Puts the app's recent connection log on the clipboard, with enough context to read it cold.
    private var supportSection: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("SUPPORT").s1Font(11, weight: .semibold).foregroundStyle(.secondary)
            Button(action: copyLog) {
                HStack {
                    Text(copiedLines.map { "Copied \($0) lines" } ?? "Copy log to clipboard").s1Font(14)
                    Spacer()
                    Image(systemName: copiedLines == nil ? "doc.on.doc" : "checkmark").font(.system(size: 14, weight: .light))
                }
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .accessibilityLabel("Copy log to clipboard")
        }
        .padding(16).background(S1Palette.panel(scheme)).padding(.top, 12)
    }
    private func copyLog() {
        #if canImport(UIKit)
        let info = Bundle.main.infoDictionary
        let header = [
            "App: \(info?["CFBundleShortVersionString"] as? String ?? "?") (\(info?["CFBundleVersion"] as? String ?? "?"))",
            "iOS: \(UIDevice.current.systemVersion), \(UIDevice.current.model)",
            "Daemon: \(store.daemonURLString)",
            "Status: \(store.connectionStatus.rawValue)\(store.streamNote.map { " (\($0))" } ?? "")",
            "Rooms: \(store.rooms.count)"
        ]
        UIPasteboard.general.string = AppLog.shared.report(header: header)
        copiedLines = AppLog.shared.count
        Task {
            try? await Task.sleep(for: .seconds(2.5))
            copiedLines = nil
        }
        #endif
    }
    var body: some View {
        GeometryReader { geometry in
            ScrollView(.vertical, showsIndicators: false) {
                VStack(alignment: .leading, spacing: 0) {
                    if store.isLive {
                        daemonSettings
                        supportSection
                    }
                    ForEach(store.isLive ? [] : sections, id: \.0) { section in
                        Text(section.0.uppercased()).s1Font(11, weight: .semibold)
                            .foregroundStyle(S1Palette.secondary(scheme))
                            .padding(.horizontal, 16).padding(.top, 22).padding(.bottom, 8)
                        VStack(spacing: 0) {
                            ForEach(section.1, id: \.1) { row in
                                Button { detail = row.1 } label: {
                                    HStack(spacing: 12) {
                                        Image(systemName: row.0).font(.system(size: 19, weight: .light)).frame(width: 24)
                                        Text(row.1).s1Font(14)
                                        Spacer(minLength: 4)
                                        Text(row.2).s1Font(11).foregroundStyle(S1Palette.secondary(scheme)).lineLimit(1)
                                        Image(systemName: "chevron.right").font(.system(size: 11)).foregroundStyle(S1Palette.secondary(scheme))
                                    }
                                    .padding(.horizontal, 16).frame(minHeight: 50)
                                    .background(S1Palette.panel(scheme))
                                    .overlay(alignment: .bottom) { Rectangle().fill(Color.black.opacity(0.08)).frame(height: 0.5).padding(.leading, 52) }
                                    .contentShape(Rectangle())
                                }
                                .buttonStyle(.plain)
                            }
                        }
                    }
                    Text(store.isLive ? "FrankenSonos · Daemon remote" : "FrankenSonos · Mock remote").s1Font(11).foregroundStyle(S1Palette.secondary(scheme))
                        .frame(maxWidth: .infinity).padding(.vertical, 24)
                }
                .frame(width: geometry.size.width, alignment: .leading)
                .padding(.bottom, 16)
            }
            .scrollClipDisabled()
        }
        .background(S1Palette.page(scheme))
        .alert(detail ?? "Settings", isPresented: Binding(get: { detail != nil }, set: { if !$0 { detail = nil } })) {
            Button("OK") { detail = nil }
        } message: { Text("This setting is a local mock preview.") }
    }
}
