#if os(macOS) || os(iOS)
import SwiftUI
import FrankenSonosKit

@MainActor struct SonosPlaylistManagementPanel: View {
    @ObservedObject var bridge: RemoteBridge
    private var workflow: SonosPlaylistManagementWorkflow { bridge.model.playlistManagement }

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 22) {
                    if let review = workflow.review { reviewDetails(review) }
                    else if let operation = workflow.operation, let zone = workflow.zone {
                        composer(operation, in: zone)
                    }
                    if workflow.phase != .idle {
                        HStack(spacing: 12) {
                            ProgressView().controlSize(.small)
                            Text(progressText).font(.callout)
                            Spacer()
                            Button("Stop waiting") { workflow.cancel() }
                        }
                        Text("Stopping a read sends nothing. A submitted change may already have happened; its outcome stays attached to the original household.")
                            .font(.caption).foregroundStyle(.secondary)
                    }
                    if let problem = workflow.problem {
                        Label(problem, systemImage: "exclamationmark.circle")
                            .font(.callout).foregroundStyle(.red).textSelection(.enabled)
                    }
                    if let result = workflow.result { resultDetails(result) }
                    if let observed = workflow.recovery {
                        VStack(alignment: .leading, spacing: 12) {
                            Label("Original household observed", systemImage: "eye")
                                .font(.headline)
                            Text("\(observed.target.address) · \(observed.observedAt.formatted(date: .omitted, time: .shortened))")
                                .font(.caption).foregroundStyle(.secondary)
                            Text(observed.detail).font(.callout).textSelection(.enabled)
                            if observed.catalog.isEmpty { Text("No saved Sonos playlists are currently listed.").font(.callout) }
                            ForEach(observed.catalog) { item in
                                HStack(alignment: .firstTextBaseline) {
                                    Text(item.title).frame(maxWidth: .infinity, alignment: .leading)
                                    Text(item.id).font(.caption.monospaced()).foregroundStyle(.secondary)
                                }
                            }
                        }.remoteCard()
                    }
                }.padding(20).frame(maxWidth: 650, alignment: .leading)
                    .frame(maxWidth: .infinity)
            }
            .navigationTitle("Saved Sonos playlists")
            .toolbar { ToolbarItem(placement: .cancellationAction) { Button("Close") { workflow.close() } } }
        }.frame(minWidth: 280, minHeight: 360)
    }

    private var progressText: String {
        switch workflow.phase {
        case .idle: ""
        case .preparing: "Reading the original group and saved catalog…"
        case .confirming: "Checking the review and sending one change…"
        case .checkingCatalog: "Reading the original household's saved playlists…"
        }
    }

    @ViewBuilder private func composer(_ operation: SonosPlaylistMutation.Operation, in zone: Zone) -> some View {
        VStack(alignment: .leading, spacing: 14) {
            Text(operation == .save ? "Keep this queue for another day" : operation == .rename ? "Give this playlist a new name" : "Delete this saved playlist")
                .font(.title2.bold()).accessibilityAddTraits(.isHeader)
            Label(zone.id.address, systemImage: "hifispeaker.2").font(.headline)
            Text(zone.members.joined(separator: " + ")).font(.caption).foregroundStyle(.secondary)
            if let playlist = workflow.playlist {
                Text("\(playlist.title) · \(playlist.id)").font(.callout).textSelection(.enabled)
            }
            if operation != .remove {
                TextField(operation == .save ? "New playlist title" : "New title", text: Binding(
                    get: { workflow.title }, set: { workflow.editTitle($0) }))
                    .textFieldStyle(.roundedBorder).disabled(!workflow.canEdit)
                Text("Titles may be shared by several saved playlists. Each native ID remains separate.")
                    .font(.caption).foregroundStyle(.secondary)
            } else {
                Text("This removes the saved playlist permanently. Its current playing queue stays in place. You will review the exact saved ID and current track count before deletion.")
                    .font(.callout)
            }
            Button("Review \(operation == .save ? "save" : operation == .rename ? "rename" : "deletion")") { workflow.prepare() }
                .buttonStyle(.borderedProminent).disabled(!workflow.canReview)
        }
    }

    private func reviewDetails(_ review: SonosPlaylistMutationReview) -> some View {
        VStack(alignment: .leading, spacing: 14) {
            Text(review.command.title).font(.title2.bold()).accessibilityAddTraits(.isHeader)
            Text(review.command.zone.id.address).font(.headline)
            Text(review.command.zone.members.joined(separator: " + ")).font(.caption).foregroundStyle(.secondary)
            if let item = review.command.playlist { Text("Saved identity: \(item.id) · \(item.title)").textSelection(.enabled) }
            if let title = review.command.newTitle { Text("New title: \(title)").font(.headline).textSelection(.enabled) }
            Text("\(review.trackCount) \(review.command.operation == .save ? "retained queue" : "saved") \(review.trackCount == 1 ? "entry" : "entries")")
            if let first = review.firstTrack { Text("First entry: \(first)").font(.callout).foregroundStyle(.secondary) }
            if let source = review.currentSource {
                Text("Currently playing source: \(source). The retained queue is what will be saved.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            if review.command.operation == .save && review.duplicateTitles > 0 {
                Label("\(review.duplicateTitles) existing playlist(s) already use this title. This save creates another separate playlist.", systemImage: "info.circle")
                    .font(.callout)
            }
            Text(review.command.explanation).font(.callout).textSelection(.enabled)
            Text("The current group and revision are read again when you confirm. Client reads cannot reserve a speaker against changes from another controller.")
                .font(.caption).foregroundStyle(.secondary)
            HStack(spacing: 12) {
                Button("Back") { workflow.cancel() }.buttonStyle(.bordered)
                Button(review.command.operation == .save ? "Save new playlist" : review.command.operation == .rename ? "Rename playlist" : "Delete playlist",
                       role: review.command.operation == .remove ? .destructive : nil) { workflow.confirm(review.id) }
                    .buttonStyle(.borderedProminent)
            }.disabled(!workflow.active || workflow.phase != .idle)
        }
    }

    private func resultDetails(_ result: SonosPlaylistManagementResult) -> some View {
        VStack(alignment: .leading, spacing: 12) {
            Label(result.title, systemImage: result.kind == .confirmed ? "checkmark.circle" : "exclamationmark.triangle")
                .font(.headline)
            Text("Original target: \(result.command.zone.id.address)").font(.caption).foregroundStyle(.secondary)
            Text(result.detail).font(.callout).textSelection(.enabled)
            if result.kind != .notSent {
                Button("Check original household's saved playlists", systemImage: "arrow.clockwise") { workflow.checkCatalog() }
                    .buttonStyle(.bordered).disabled(!workflow.canCheckCatalog)
            }
            if workflow.needsRefresh {
                Text("A successful explicit catalog read is required before another saved-playlist change. Playback controls remain available after this request finishes.")
                    .font(.caption).foregroundStyle(.secondary)
            }
        }.remoteCard()
    }
}

@MainActor struct SonosPlaylistManagementNotice: View {
    @ObservedObject var bridge: RemoteBridge
    private var workflow: SonosPlaylistManagementWorkflow { bridge.model.playlistManagement }
    var body: some View {
        if let result = workflow.result {
            VStack(alignment: .leading, spacing: 10) {
                Text(result.title).font(.headline)
                Text("Saved playlist · \(result.command.zone.id.address)").font(.caption).foregroundStyle(.secondary)
                Text(result.detail).font(.callout).textSelection(.enabled)
                HStack(spacing: 12) {
                    Button("Inspect playlist result") { workflow.openResult() }
                        .buttonStyle(.bordered).disabled(!bridge.model.canSchedule)
                    Button("Dismiss") { workflow.dismissResult() }
                        .disabled(workflow.isEngaged || workflow.needsRefresh)
                }
            }.remoteCard()
        }
    }
}
#endif
