#if os(macOS) || os(iOS)
import SwiftUI
import FrankenSonosKit

@MainActor struct FavoriteManagementPanel: View {
    @ObservedObject var bridge: RemoteBridge
    private var workflow: FavoriteManagementWorkflow { bridge.model.favoriteManagement }

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
                        Text("A submitted change may already have happened. Its result stays attached to the original household, even if you close this screen.")
                            .font(.caption).foregroundStyle(.secondary)
                    }
                    if let problem = workflow.problem {
                        Label(problem, systemImage: "exclamationmark.circle")
                            .font(.callout).foregroundStyle(.red).textSelection(.enabled)
                    }
                    if let result = workflow.result { resultDetails(result) }
                    if let observed = workflow.recovery { recoveryDetails(observed) }
                }.padding(20).frame(maxWidth: 650, alignment: .leading)
                    .frame(maxWidth: .infinity)
            }
            .navigationTitle("My Sonos favorites")
            .toolbar { ToolbarItem(placement: .cancellationAction) { Button("Close") { workflow.close() } } }
        }.frame(minWidth: 280, minHeight: 360)
    }

    private var progressText: String {
        switch workflow.phase {
        case .idle: ""
        case .preparing: "Reading the selected source and this household's favorites…"
        case .confirming: "Checking the review and saving your change…"
        case .checkingCatalog: "Reading the original household's favorites…"
        }
    }

    @ViewBuilder private func composer(_ operation: FavoriteMutation.Operation, in zone: Zone) -> some View {
        VStack(alignment: .leading, spacing: 14) {
            Text(operation == .save ? "Keep this track or station" : operation == .rename ? "Give this favorite a new name" : "Remove this favorite")
                .font(.title2.bold()).accessibilityAddTraits(.isHeader)
            destination(zone)
            if let favorite = workflow.favorite {
                sourceDetails(favorite, current: false)
            }
            if operation != .remove {
                TextField(operation == .save ? "Favorite title" : "New title", text: Binding(
                    get: { workflow.title }, set: { workflow.editTitle($0) }))
                    .textFieldStyle(.roundedBorder).disabled(!workflow.canEdit)
                    .accessibilityLabel(operation == .save ? "Favorite title" : "New favorite title")
                Text(operation == .save
                    ? "Name your favorite, then review the track or station currently selected in this group."
                    : "The favorite keeps its current source. You will review its exact identity before renaming it.")
                    .font(.callout).foregroundStyle(.secondary)
                Text("Different favorites can share a title. Saving creates a separate favorite.")
                    .font(.caption).foregroundStyle(.secondary)
            } else {
                Text("This permanently removes the saved favorite from My Sonos. Music already playing or queued continues. You will review the exact favorite before removal.")
                    .font(.callout)
            }
            Button(operation == .save ? "Review current source" : operation == .rename ? "Review rename" : "Review removal") {
                workflow.prepare()
            }.buttonStyle(.borderedProminent).disabled(!workflow.canReview)
        }
    }

    private func destination(_ zone: Zone) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Label("My Sonos · \(zone.household)", systemImage: "heart")
                .font(.headline)
            Text("Group: \(zone.members.joined(separator: " + "))")
                .font(.callout).foregroundStyle(.secondary)
            Text(zone.id.address).font(.caption).foregroundStyle(.secondary)
        }.accessibilityElement(children: .combine)
    }

    private func sourceDetails(_ favorite: Favorite, current: Bool) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Label("\(current ? "Current " : "")\(sourceKind(favorite.kind))",
                  systemImage: favorite.kind == "stream" ? "radio" : "music.note")
                .font(.caption).foregroundStyle(.secondary)
            Text(favorite.title).font(.headline).textSelection(.enabled)
            if let description = favorite.description, !description.isEmpty {
                Text(description).font(.callout).foregroundStyle(.secondary).textSelection(.enabled)
            }
            if !favorite.id.isEmpty {
                Text("Favorite ID: \(favorite.id)").font(.caption.monospaced())
                    .foregroundStyle(.secondary).textSelection(.enabled)
            }
        }.frame(maxWidth: .infinity, alignment: .leading).remoteCard()
    }

    private func sourceKind(_ kind: String) -> String {
        switch kind {
        case "track": "track"
        case "stream": "station"
        case "container": "album or playlist"
        default: "saved source"
        }
    }

    private func reviewDetails(_ review: FavoriteMutationReview) -> some View {
        VStack(alignment: .leading, spacing: 14) {
            Text(review.command.title).font(.title2.bold()).accessibilityAddTraits(.isHeader)
            destination(review.command.zone)
            sourceDetails(review.favorite, current: review.command.operation == .save)
            if let title = review.command.newTitle {
                Text("\(review.command.operation == .save ? "Save as" : "New title"): \(title)")
                    .font(.headline).textSelection(.enabled)
            }
            if review.command.operation == .save && review.duplicateTitles > 0 {
                Label("\(review.duplicateTitles) existing favorite(s) already use this title. This save creates another separate favorite.", systemImage: "info.circle")
                    .font(.callout)
            }
            Text(review.command.explanation).font(.callout).textSelection(.enabled)
            Text("The source and group are checked again when you confirm. If they changed, you will be asked to review again.")
                .font(.caption).foregroundStyle(.secondary)
            HStack(spacing: 12) {
                Button("Back") { workflow.cancel() }.buttonStyle(.bordered)
                Button(review.command.operation == .save ? "Save favorite" : review.command.operation == .rename ? "Rename favorite" : "Remove favorite",
                       role: review.command.operation == .remove ? .destructive : nil) { workflow.confirm(review.id) }
                    .buttonStyle(.borderedProminent)
            }.disabled(!workflow.active || workflow.phase != .idle)
        }
    }

    private func resultDetails(_ result: FavoriteManagementResult) -> some View {
        VStack(alignment: .leading, spacing: 12) {
            Label(result.title, systemImage: result.kind == .confirmed ? "checkmark.circle" : "exclamationmark.triangle")
                .font(.headline)
            Text("Original target: \(result.command.zone.id.address)")
                .font(.caption).foregroundStyle(.secondary)
            Text(result.detail).font(.callout).textSelection(.enabled)
            if result.kind != .notSent {
                Button("Check original household's favorites", systemImage: "arrow.clockwise") { workflow.checkCatalog() }
                    .buttonStyle(.bordered).disabled(!workflow.canCheckCatalog)
            }
            if workflow.needsRefresh {
                Text("Check the original household's favorites before making another favorite change. Playback controls remain available after this request finishes.")
                    .font(.caption).foregroundStyle(.secondary)
            }
        }.remoteCard()
    }

    private func recoveryDetails(_ observed: FavoriteCatalogRecovery) -> some View {
        VStack(alignment: .leading, spacing: 12) {
            Label("Original household observed", systemImage: "eye").font(.headline)
            Text("\(observed.target.address) · \(observed.observedAt.formatted(date: .omitted, time: .shortened))")
                .font(.caption).foregroundStyle(.secondary)
            Text(observed.detail).font(.callout).textSelection(.enabled)
            if observed.catalog.isEmpty { Text("No My Sonos favorites are currently listed.").font(.callout) }
            ForEach(observed.catalog) { favorite in
                VStack(alignment: .leading, spacing: 4) {
                    Text(favorite.title).font(.callout).textSelection(.enabled)
                    Text("\(sourceKind(favorite.kind)) · \(favorite.id)")
                        .font(.caption).foregroundStyle(.secondary).textSelection(.enabled)
                }.frame(maxWidth: .infinity, alignment: .leading)
            }
        }.remoteCard()
    }
}

@MainActor struct FavoriteManagementNotice: View {
    @ObservedObject var bridge: RemoteBridge
    private var workflow: FavoriteManagementWorkflow { bridge.model.favoriteManagement }

    var body: some View {
        if let result = workflow.result {
            VStack(alignment: .leading, spacing: 10) {
                Label(result.title, systemImage: result.kind == .confirmed ? "checkmark.circle" : "exclamationmark.triangle")
                    .font(.headline)
                Text("My Sonos favorites · \(result.command.zone.id.address)")
                    .font(.caption).foregroundStyle(.secondary)
                Text(result.detail).font(.callout).textSelection(.enabled)
                if workflow.needsRefresh {
                    Text("Inspect this result to check the original household before making another favorite change.")
                        .font(.caption).foregroundStyle(.secondary)
                }
                HStack(spacing: 12) {
                    Button("Inspect favorite result") { workflow.openResult() }
                        .buttonStyle(.bordered).disabled(!bridge.model.canSchedule)
                    Button("Dismiss") { workflow.dismissResult() }
                        .disabled(workflow.isEngaged || workflow.needsRefresh)
                }
            }.remoteCard()
        } else if !workflow.isOpen && workflow.phase == .confirming, let zone = workflow.zone {
            VStack(alignment: .leading, spacing: 10) {
                ProgressView("Finishing favorite change…")
                Text("My Sonos favorites · \(zone.id.address)")
                    .font(.caption).foregroundStyle(.secondary)
                Text("The result will remain here when this request finishes.").font(.callout)
            }.remoteCard()
        }
    }
}
#endif
