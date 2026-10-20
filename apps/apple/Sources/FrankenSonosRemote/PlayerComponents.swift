#if os(macOS) || os(iOS)
import SwiftUI
import FrankenSonosKit
#if os(iOS)
import UIKit
#else
import AppKit
#endif

@MainActor enum RemoteStyle {
    static var accent: Color {
        adaptive(light: (0.192, 0.337, 0.129), dark: (0.824, 0.937, 0.604))
    }
    static var accentInk: Color {
        adaptive(light: (1, 0.996, 0.973), dark: (0.09, 0.137, 0.047))
    }
    static var background: Color {
        adaptive(light: (0.957, 0.953, 0.925), dark: (0.067, 0.082, 0.063))
    }
    static var surface: Color {
        adaptive(light: (1, 0.996, 0.973), dark: (0.106, 0.129, 0.102))
    }
    private static func adaptive(light: (Double, Double, Double), dark: (Double, Double, Double)) -> Color {
        #if os(iOS)
        Color(uiColor: UIColor { traits in
            let color = traits.userInterfaceStyle == .dark ? dark : light
            return UIColor(red: CGFloat(color.0), green: CGFloat(color.1), blue: CGFloat(color.2), alpha: 1)
        })
        #else
        Color(nsColor: NSColor(name: nil) { appearance in
            let color = appearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua ? dark : light
            return NSColor(srgbRed: CGFloat(color.0), green: CGFloat(color.1), blue: CGFloat(color.2), alpha: 1)
        })
        #endif
    }
}

extension View {
    @MainActor func remoteCard() -> some View {
        padding(18)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(RemoteStyle.surface, in: RoundedRectangle(cornerRadius: 22))
    }
}

@MainActor struct PlayerArtwork: View {
    let url: URL?
    var cornerRadius: CGFloat = 24

    var body: some View {
        AsyncImage(url: url) { phase in
            if case .success(let image) = phase {
                image.resizable().scaledToFill()
            } else {
                ZStack {
                    LinearGradient(colors: [Color(red: 0.13, green: 0.18, blue: 0.11),
                                            Color(red: 0.32, green: 0.40, blue: 0.23)],
                                   startPoint: .topLeading, endPoint: .bottomTrailing)
                    GeometryReader { proxy in
                        Image(systemName: "music.note")
                            .font(.system(size: proxy.size.width * 0.28, weight: .light))
                            .foregroundStyle(.white.opacity(0.72))
                            .frame(maxWidth: .infinity, maxHeight: .infinity)
                    }
                }
            }
        }
        .aspectRatio(1, contentMode: .fit)
        .clipShape(RoundedRectangle(cornerRadius: cornerRadius))
        .accessibilityHidden(true)
    }
}

@MainActor struct PlayerTimeline: View {
    let timeline: PlaybackTimeline
    let enabled: Bool
    let submit: (UInt32) -> Void
    @State private var draft: Double?

    var body: some View {
        TimelineView(.periodic(from: Date(), by: 1)) { context in
            let position = timeline.position(at: context.date) ?? 0
            let maximum = max(Double(timeline.duration ?? 0) - 1, 1)
            VStack(alignment: .leading, spacing: 5) {
                Slider(value: Binding(get: { min(draft ?? position, maximum) }, set: { draft = $0 }),
                       in: 0...maximum, step: 1)
                    .disabled(!enabled || !timeline.canSeek || (timeline.duration ?? 0) <= 1)
                    .accessibilityLabel("Playback position")
                    .accessibilityValue(PlaybackTimeline.format(draft ?? position))
                    .accessibilityHint("Choose a position, then review the seek.")
                HStack {
                    Text(draft.map(PlaybackTimeline.format) ?? timeline.elapsedLabel(at: context.date))
                    Spacer()
                    if let duration = timeline.duration, duration > 0 {
                        Text(PlaybackTimeline.format(Double(duration)))
                    } else {
                        Text("Duration unavailable")
                    }
                }.font(.caption.monospacedDigit()).foregroundStyle(.secondary)
                if let draft {
                    ViewThatFits(in: .horizontal) {
                        HStack { seekButtons(draft) }
                        VStack(alignment: .leading) { seekButtons(draft) }
                    }
                    .padding(.top, 8)
                }
            }
        }
    }

    @ViewBuilder private func seekButtons(_ seconds: Double) -> some View {
        Button("Seek to \(PlaybackTimeline.format(seconds))") {
            if let value = timeline.seekPosition(seconds: seconds) {
                submit(value); draft = nil
            }
        }
        .buttonStyle(.borderedProminent).foregroundStyle(RemoteStyle.accentInk)
        .disabled(!enabled || !timeline.canSeek)
        .accessibilityHint("Opens a review before changing playback.")
        Button("Cancel seek") { draft = nil }.buttonStyle(.bordered)
    }
}

@MainActor struct RoomLevelControl: View {
    let title: String
    let level: Int
    let enabled: Bool
    let submit: (Int) -> Void
    @State private var value: Double
    @State private var hasDraft = false

    init(title: String, level: Int, enabled: Bool, submit: @escaping (Int) -> Void) {
        self.title = title; self.level = level; self.enabled = enabled; self.submit = submit
        _value = State(initialValue: Double(level))
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(alignment: .firstTextBaseline) {
                Text(title).font(.headline)
                Spacer()
                Text("\(level)%").font(.subheadline.monospacedDigit()).foregroundStyle(.secondary)
            }
            HStack(spacing: 12) {
                Image(systemName: "speaker.fill").foregroundStyle(.secondary).accessibilityHidden(true)
                Slider(value: Binding(get: { value }, set: { value = $0; hasDraft = true }), in: 0...100, step: 1)
                    .disabled(!enabled)
                    .accessibilityLabel("\(title) volume")
                    .accessibilityValue("\(Int(value)) percent, current volume \(level) percent")
                    .accessibilityHint("Choose a level, then review the volume change.")
                Image(systemName: "speaker.wave.3.fill").foregroundStyle(.secondary).accessibilityHidden(true)
            }
            if hasDraft && Int(value) != level {
                ViewThatFits(in: .horizontal) {
                    HStack { draftButtons }
                    VStack(alignment: .leading) { draftButtons }
                }
            }
        }
        .onChange(of: level) { _, current in
            if !hasDraft || Int(value) == current { value = Double(current); hasDraft = false }
        }
    }

    @ViewBuilder private var draftButtons: some View {
        Button("Review \(Int(value))% volume") { submit(Int(value)) }
            .buttonStyle(.borderedProminent).foregroundStyle(RemoteStyle.accentInk).disabled(!enabled)
        Button("Cancel") { value = Double(level); hasDraft = false }.buttonStyle(.bordered)
    }
}

@MainActor struct EmptyMusicState: View {
    let title: String
    let systemImage: String
    let detail: String
    var body: some View {
        ContentUnavailableView(title, systemImage: systemImage, description: Text(detail))
            .frame(maxWidth: .infinity).padding(.vertical, 12)
    }
}

@MainActor struct ReviewActions: View {
    let confirm: () -> Void
    let cancel: () -> Void
    var body: some View {
        ViewThatFits(in: .horizontal) {
            HStack(spacing: 12) { buttons }
            VStack(spacing: 12) { buttons }
        }
        .padding(20).frame(maxWidth: .infinity).background(.bar)
    }

    @ViewBuilder private var buttons: some View {
        Button(action: cancel) { Text("Cancel").frame(maxWidth: .infinity, minHeight: 32) }
            .buttonStyle(.bordered).keyboardShortcut(.cancelAction)
        Button(action: confirm) { Text("Confirm once").frame(maxWidth: .infinity, minHeight: 32) }
            .buttonStyle(.borderedProminent).foregroundStyle(RemoteStyle.accentInk)
    }
}
#endif
