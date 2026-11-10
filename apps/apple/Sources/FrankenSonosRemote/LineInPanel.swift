#if os(macOS) || os(iOS)
import SwiftUI
import FrankenSonosKit

@MainActor struct LineInPanel: View {
    @ObservedObject var bridge: RemoteBridge
    private var model: RemoteModel { bridge.model }

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("Play a turntable or another source connected to a speaker in this Sonos household.")
                .font(.subheadline).foregroundStyle(.secondary)
            if let catalog = model.lineInCatalog, let zone = model.selectedZone {
                if catalog.sources.isEmpty && !model.panelLoading {
                    VStack(alignment: .leading, spacing: 6) {
                        Label(catalog.unavailable.isEmpty ? "No line-in inputs advertised" : "No inputs confirmed",
                              systemImage: "cable.connector").font(.headline)
                        Text(catalog.unavailable.isEmpty
                            ? "This household has no discovered speakers advertising a physical line-in input."
                            : "Some players could not be read. Refresh when those speakers are available.")
                            .font(.callout).foregroundStyle(.secondary)
                    }.remoteCard()
                }
                ForEach(catalog.sources) { source in
                    HStack(alignment: .top, spacing: 12) {
                        Image(systemName: "cable.connector").font(.title2).foregroundStyle(RemoteStyle.accent)
                            .accessibilityHidden(true)
                        VStack(alignment: .leading, spacing: 5) {
                            Text(source.displayName).font(.headline)
                            Text("\(source.room) · \(zone.household)").font(.subheadline).foregroundStyle(.secondary)
                            // A stereo room can expose two different physical jacks.
                            Text(source.player).font(.caption2.monospaced()).foregroundStyle(.secondary)
                                .textSelection(.enabled)
                        }.frame(maxWidth: .infinity, alignment: .leading)
                        Button("Play now", systemImage: "play.fill") { model.prepareLineIn(source, in: zone) }
                            .buttonStyle(.bordered).frame(minHeight: 44)
                            .disabled(!model.canPrepare || model.panelLoading || model.panelProblem != nil)
                            .accessibilityLabel("Play \(source.displayName) from \(source.room)")
                            .accessibilityHint("Review this exact input and the selected destination group before playing.")
                    }.remoteCard()
                }
                if !catalog.unavailable.isEmpty {
                    VStack(alignment: .leading, spacing: 10) {
                        Label("Some players could not be read", systemImage: "exclamationmark.circle").font(.headline)
                        ForEach(catalog.unavailable) { player in
                            VStack(alignment: .leading, spacing: 4) {
                                Text(player.room).font(.subheadline.weight(.semibold))
                                Text(player.detail).font(.caption).foregroundStyle(.secondary)
                                    .textSelection(.enabled).fixedSize(horizontal: false, vertical: true)
                            }
                        }
                    }.remoteCard()
                }
            }
            Text("Cable connection and audio signal are unknown. Connect and start your source before playing. The selected group keeps its queue and room membership.")
                .font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
        }
    }
}
#endif
