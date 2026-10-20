#if os(macOS) || os(iOS)
import SwiftUI
import FrankenSonosKit

@MainActor struct SleepPanel: View {
    @ObservedObject var bridge: RemoteBridge
    @State private var minutes = "45"
    private var model: RemoteModel { bridge.model }
    private var seconds: UInt32? { SleepChange.durationSeconds(minutes: minutes) }
    private var ready: Bool { model.canPrepare && !model.panelLoading }

    var body: some View {
        let zone = model.selectedZone
        VStack(alignment: .leading, spacing: 20) {
            Text("Let the music wind down. Your timer keeps running when you close the app.")
                .font(.callout).foregroundStyle(.secondary)
            if let readAt = model.sleepObservedAt {
                VStack(alignment: .leading, spacing: 12) {
                    Label(model.sleepTimer == nil ? "No timer set" : "Winding down", systemImage: "moon.zzz.fill")
                        .font(.headline).foregroundStyle(RemoteStyle.accent)
                    if let timer = model.sleepTimer {
                        if let deadline = timer.deadline {
                            TimelineView(.periodic(from: Date(), by: 1)) { context in
                                if let remaining = timer.estimatedRemaining(at: context.date), remaining > 0 {
                                    Text(PlaybackTimeline.format(Double(remaining)))
                                        .font(.largeTitle.bold().monospacedDigit())
                                        .accessibilityLabel("Estimated time remaining: \(remaining / 60) minutes and \(remaining % 60) seconds")
                                } else {
                                    Text("The reported deadline has arrived. Refresh to check playback.").font(.callout)
                                }
                            }
                            Text("Pauses at \(deadline.formatted(date: .omitted, time: .shortened)) on this device")
                                .font(.subheadline).foregroundStyle(.secondary)
                        } else { Text(timer.done).font(.callout).textSelection(.enabled) }
                        Text(timer.fading ? "Fading now" : (timer.fades ? "A gentle fade before pausing" : "Speaker timer · pauses without a fade"))
                            .font(.caption).foregroundStyle(.secondary)
                    } else {
                        Text("Choose how long you’d like to keep listening.").foregroundStyle(.secondary)
                    }
                    Text("Last checked \(readAt.formatted(date: .omitted, time: .shortened))")
                        .font(.caption).foregroundStyle(.secondary)
                }.remoteCard()
            } else if !model.panelLoading {
                Label("Timer state is unavailable. Refresh to check this group.", systemImage: "exclamationmark.circle")
                    .font(.callout).foregroundStyle(.secondary)
            }
            VStack(alignment: .leading, spacing: 16) {
                Text("Pause after").font(.headline)
                LazyVGrid(columns: [GridItem(.adaptive(minimum: 64), spacing: 8)], spacing: 8) {
                    ForEach([15, 30, 45, 60, 90], id: \.self) { value in
                        Button { minutes = String(value) } label: {
                            Text("\(value)m").font(.subheadline.weight(.medium))
                                .frame(maxWidth: .infinity, minHeight: 44)
                                .foregroundStyle(minutes == String(value) ? RemoteStyle.accentInk : RemoteStyle.accent)
                                .background(minutes == String(value) ? RemoteStyle.accent : RemoteStyle.accent.opacity(0.1),
                                            in: RoundedRectangle(cornerRadius: 12))
                        }.buttonStyle(.plain).accessibilityLabel("\(value) minutes")
                            .accessibilityValue(minutes == String(value) ? "Selected" : "")
                    }
                }.disabled(!ready)
                HStack {
                    Text("Custom duration")
                    Spacer()
                    TextField("Minutes", text: $minutes)
                        .textFieldStyle(.roundedBorder).frame(width: 78)
                        .accessibilityLabel("Custom sleep timer minutes")
                        #if os(iOS)
                        .keyboardType(.numberPad)
                        #endif
                    Text("min").foregroundStyle(.secondary)
                }
                if seconds == nil {
                    Text("Choose a whole number from 1 to 1380 minutes.").font(.caption).foregroundStyle(.secondary)
                }
                Button {
                    if let seconds { model.prepare(.sleep(.set(seconds: seconds)), in: zone) }
                } label: {
                    Label(model.sleepTimer == nil ? "Review timer" : "Replace timer", systemImage: "moon")
                        .frame(maxWidth: .infinity, minHeight: 32)
                }
                .buttonStyle(.borderedProminent).foregroundStyle(RemoteStyle.accentInk)
                .disabled(!ready || seconds == nil)
                if model.sleepTimer != nil {
                    Button("Add \(minutes) minutes to this timer") {
                        if let seconds { model.prepare(.sleep(.extend(seconds: seconds)), in: zone) }
                    }.buttonStyle(.bordered).disabled(!ready || seconds == nil)
                    Button("Cancel timer", systemImage: "xmark.circle") { model.prepare(.sleep(.cancel), in: zone) }
                        .buttonStyle(.bordered).disabled(!ready)
                    Text("Adding time extends the existing deadline. Cancelling the timer does not resume music.")
                        .font(.caption).foregroundStyle(.secondary)
                }
            }.remoteCard()
        }
    }
}
#endif
