#if os(macOS)
import SwiftUI
import FrankenSonosKit

@MainActor struct SleepPanel: View {
    @ObservedObject var bridge: RemoteBridge
    @State private var minutes = "45"
    private var model: RemoteModel { bridge.model }
    private var seconds: UInt32? { SleepChange.durationSeconds(minutes: minutes) }

    var body: some View {
        let zone = model.selectedZone
        VStack(alignment: .leading, spacing: 14) {
            Text("The timer belongs to this group and runs on the daemon or speaker, not in this window. Closing the remote does not cancel it.")
                .font(.callout).foregroundStyle(.secondary)
            if let readAt = model.sleepObservedAt {
                if let timer = model.sleepTimer {
                    Text(timer.done).font(.headline).textSelection(.enabled)
                    if let deadline = timer.deadline {
                        Text("Pause time on this Mac: \(deadline.formatted(date: .abbreviated, time: .standard))")
                        TimelineView(.periodic(from: Date(), by: 1)) { context in
                            if let seconds = timer.estimatedRemaining(at: context.date), seconds > 0 {
                                Text("Estimated remaining: \(seconds / 60)m \(seconds % 60)s").monospacedDigit()
                            } else {
                                Text("The reported deadline has arrived. Refresh to check the timer and playback; zero is not an acknowledgement.")
                            }
                        }
                    }
                    Text(timer.fading ? "Fade in progress" : (timer.fades ? "Daemon fade enabled" : "Speaker-only timer — no fade"))
                } else { Text("No sleep timer was reported for this group.").font(.headline) }
                Text("Timer last read: \(readAt.formatted(date: .omitted, time: .standard))")
                    .font(.caption).foregroundStyle(.secondary)
            } else if !model.panelLoading {
                Text("Timer state is unavailable. Refresh before changing an existing timer.")
            }
            HStack {
                TextField("Minutes (1–1380)", text: $minutes)
                    .textFieldStyle(.roundedBorder).frame(width: 170)
                    .accessibilityLabel("Sleep timer minutes")
                Button("Review new timer") {
                    if let seconds { model.prepare(.sleep(.set(seconds: seconds)), in: zone) }
                }
                .disabled(!model.canPrepare || model.panelLoading || seconds == nil)
            }
            if seconds == nil { Text("Enter a whole number of minutes from 1 to 1380 (23 hours).").font(.caption) }
            HStack {
                Button("Review extension") {
                    if let seconds { model.prepare(.sleep(.extend(seconds: seconds)), in: zone) }
                }.disabled(seconds == nil)
                Button("Review cancellation") { model.prepare(.sleep(.cancel), in: zone) }
            }
            .disabled(!model.canPrepare || model.panelLoading || model.sleepTimer == nil || model.sleepObservedAt == nil)
            Text("An extension adds to the existing deadline. A new timer replaces it. Cancellation does not resume music. Confirmation re-reads the timer; an uncertain write is never repeated.")
                .font(.caption).foregroundStyle(.secondary)
        }
    }
}
#endif
