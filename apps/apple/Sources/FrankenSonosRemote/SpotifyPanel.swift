#if os(macOS) || os(iOS)
import SwiftUI
import FrankenSonosKit

@MainActor struct SpotifySetupButton: View {
    @ObservedObject var bridge: RemoteBridge
    @Binding var isPresented: Bool
    var body: some View {
        Button("Spotify & Library", systemImage: "music.note.list") { isPresented = true }
            .disabled(!bridge.model.canSchedule)
            .help("Connect the daemon to your Spotify library. No selected speaker is required.")
    }
}

@MainActor struct SpotifyPanel: View {
    @ObservedObject var bridge: RemoteBridge
    @Environment(\.dismiss) private var dismiss
    @Environment(\.openURL) private var openURL
    @State private var callback = ""
    @State private var replacePending = false
    @State private var browserFailure: String?
    @State private var browserRequest = UUID()
    private var model: SpotifySetupModel { bridge.spotify }

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack {
                Label("Spotify & your DJ library", systemImage: "music.note.list").font(.title2.bold())
                Spacer()
                Button { dismiss() } label: { Image(systemName: "xmark").frame(width: 44, height: 44) }
                    .buttonStyle(.plain).keyboardShortcut(.cancelAction).accessibilityLabel("Close Spotify setup")
            }
            Text(bridge.endpoint).font(.caption).textSelection(.enabled)
            ScrollView {
                VStack(alignment: .leading, spacing: 18) {
                    if !model.active { Text("Setup suspended. Displayed observations may be stale.") }
                    if let result = model.result {
                        GroupBox(result.title) {
                            VStack(alignment: .leading, spacing: 8) {
                                if result.uncertain { Label("Refresh cannot acknowledge or repeat a lost request", systemImage: "exclamationmark.triangle") }
                                Text(result.detail).textSelection(.enabled)
                                Button("Dismiss result") { model.dismissResult() }
                            }.frame(maxWidth: .infinity, alignment: .leading)
                        }
                    }
                    signIn
                    library
                    Text("These actions affect the selected daemon's account and cache, not a selected room. They do not start music, change your Spotify library, or configure the speakers' Spotify service linkage.")
                        .font(.callout).foregroundStyle(.secondary)
                }.frame(maxWidth: .infinity, alignment: .leading)
            }
            HStack {
                if model.activity != .idle { ProgressView().controlSize(.small); Text(activity) }
                else if let at = model.observedAt { Text("Last status check: \(at.formatted(date: .omitted, time: .standard))").font(.caption) }
                Spacer()
                Button("Refresh status") { model.refresh() }.disabled(!model.active || model.activity != .idle)
            }
            Text("Closing this panel stops its requests, not a library job already running on the daemon. A sent sign-in request may still take effect.")
                .font(.caption).foregroundStyle(.secondary)
        }
        .padding(20).frame(maxWidth: 760)
        .background(RemoteStyle.background)
        #if os(macOS)
        .frame(width: 700, height: 640)
        #endif
        .presentationDetents([.large])
        .presentationDragIndicator(.visible)
        .onAppear { model.resume() }
        .onDisappear { browserRequest = UUID(); callback = ""; model.suspend() }
        .onChange(of: model.active) { _, active in if !active { callback = "" } }
        .onChange(of: model.consent) { _, consent in if consent == nil { callback = "" } }
        .onChange(of: callback) { _, text in if text.utf8.count > 16_384 { callback = "" } }
        .confirmationDialog("Replace the pending Spotify sign-in?", isPresented: $replacePending, titleVisibility: .visible) {
            Button("Replace pending sign-in") { callback = ""; model.beginSignIn(replacePending: true) }
            Button("Keep existing sign-in", role: .cancel) { }
        } message: {
            Text("The earlier browser flow will no longer complete. This creates a new consent link on the daemon; it does not open a browser automatically.")
        }
    }

    private var signIn: some View {
        GroupBox("1. Connect Spotify on the daemon") {
            VStack(alignment: .leading, spacing: 10) {
                if let state = model.signIn {
                    Label(state.signedIn ? "The daemon has a cached Spotify grant" : "The daemon has no cached Spotify grant",
                          systemImage: state.signedIn ? "checkmark.circle" : "person.crop.circle.badge.questionmark")
                    Text("Cached sign-in alone does not verify token validity or a completed library import.").font(.caption).foregroundStyle(.secondary)
                    Text("Registered return address: \(state.redirectUri)").font(.caption).textSelection(.enabled)
                    if state.pending { Text("A consent flow is waiting for the browser to return.") }
                    VStack(alignment: .leading, spacing: 12) {
                        Button(state.pending ? "Replace pending sign-in…" : (state.signedIn ? "Reconnect Spotify" : "Begin Spotify sign-in")) {
                            if state.pending { replacePending = true }
                            else { callback = ""; model.beginSignIn() }
                        }.disabled(!model.canBegin)
                        if model.consent != nil {
                            Button("Open Spotify in browser", action: openSpotify)
                                .buttonStyle(.borderedProminent).foregroundStyle(RemoteStyle.accentInk).disabled(!model.canComplete)
                        }
                    }
                    if model.consent != nil {
                        Text("Authorize read-only access in Spotify, then return here. If the return address cannot be reached from this device, paste the complete address the browser landed on below. Do not share that address: it contains a one-time code.")
                            .font(.callout)
                        SecureField("Paste the complete callback address", text: $callback)
                            .textFieldStyle(.roundedBorder).accessibilityLabel("Spotify callback address, kept only until submission or closing")
                            #if os(iOS)
                            .textInputAutocapitalization(.never)
                            .autocorrectionDisabled()
                            #endif
                        Button("Complete sign-in once") {
                            let text = callback; callback = ""; model.completeSignIn(text)
                        }.disabled(!model.canComplete || callback.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                        if let problem = browserFailure ?? model.browserProblem { Text(problem).font(.callout) }
                    } else if state.pending {
                        Text("This panel has no usable consent link. Complete the existing flow in its original browser, or explicitly replace it above.").font(.callout)
                    }
                } else {
                    Text(model.signInProblem ?? "Reading the daemon's sign-in status…").textSelection(.enabled)
                    Text("Configure the Spotify client ID and registered redirect URI on the daemon host. This remote never asks for a client secret or stores an access/refresh token.")
                        .font(.caption).foregroundStyle(.secondary)
                }
            }.frame(maxWidth: .infinity, alignment: .leading).padding(4)
        }
    }

    private var library: some View {
        GroupBox("2. Refresh the DJ library") {
            VStack(alignment: .leading, spacing: 10) {
                if let sync = model.library {
                    if sync.running {
                        HStack { ProgressView().controlSize(.small); Text("A library refresh is running on the daemon") }
                    }
                    Text(sync.done).textSelection(.enabled)
                    if let error = sync.error {
                        Text((sync.running ? "Earlier attempt: " : "Latest attempt failed: ") + error).textSelection(.enabled)
                    }
                    if let last = sync.last {
                        Divider()
                        Text("Last completed pass: \(Date(timeIntervalSince1970: Double(last.at)).formatted())").font(.headline)
                        Text("\(last.tracks) tracks • \(last.candidates) DJ candidates • \(last.classical) classical • \(last.retired) retired")
                        if last.incomplete { Label("Completed with missing data", systemImage: "exclamationmark.triangle") }
                        if let pending = last.genreArtistsPending, pending > 0 { Text("\(pending) artist genre lookups remain for a later daemon pass.") }
                        ForEach(Array((last.notes ?? []).enumerated()), id: \.offset) { _, note in Text(note).font(.callout).textSelection(.enabled) }
                        if sync.running { Text("These counts belong to the earlier completed pass, not the refresh currently running.").font(.caption) }
                    } else { Text("No completed library import was reported.").foregroundStyle(.secondary) }
                    Button("Refresh library now") { model.syncLibrary() }.disabled(!model.canSync)
                } else {
                    Text(model.libraryProblem ?? "Reading library status…").textSelection(.enabled)
                }
                Text("The daemon reads your own Spotify library and updates its local cache. Future DJ picks use the refreshed pool; accepting a refresh request is not proof that it finished.")
                    .font(.caption).foregroundStyle(.secondary)
            }.frame(maxWidth: .infinity, alignment: .leading).padding(4)
        }
    }

    private func openSpotify() {
        let request = UUID()
        browserRequest = request; browserFailure = nil
        model.openConsent { url in
            openURL(url) { accepted in
                Task { @MainActor in
                    guard browserRequest == request && model.active else { return }
                    if !accepted {
                        browserFailure = "The browser could not open Spotify. Try again; no new sign-in was started."
                    }
                }
            }
            return true
        }
    }

    private var activity: String {
        switch model.activity {
        case .idle: ""
        case .reading: "Reading account and library status…"
        case .beginning: "Requesting a consent link…"
        case .completing: "Completing sign-in once…"
        case .syncing: "Requesting a library refresh…"
        }
    }
}
#endif
