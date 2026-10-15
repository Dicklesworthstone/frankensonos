#if os(macOS)
import AppKit
import SwiftUI

@main
@MainActor
enum SketchRenderer {
    static func main() throws {
        let app = NSApplication.shared
        app.setActivationPolicy(.accessory)
        let output = URL(fileURLWithPath: CommandLine.arguments.dropFirst().first ?? "sketch-shots", isDirectory: true)
        try FileManager.default.createDirectory(at: output, withIntermediateDirectories: true)
        let screens: [(String, RemoteTab, Bool, Bool, Bool, Bool, ColorScheme)] = [
            ("rooms", .rooms, false, false, false, false, .light),
            ("rooms-dark", .rooms, false, false, false, false, .dark),
            ("browse", .browse, false, false, false, false, .light),
            ("now-playing", .rooms, true, false, false, false, .dark),
            ("group-rooms", .rooms, false, true, false, false, .light),
            ("group-rooms-scrolled-end", .rooms, false, true, false, true, .light),
            ("now-playing-rooms-sheet", .rooms, true, false, true, false, .dark),
            ("now-playing-rooms-sheet-scrolled-end", .rooms, true, false, true, true, .dark),
            ("settings", .settings, false, false, false, false, .light),
            ("search", .search, false, false, false, false, .light),
            ("search-selected-room", .search, false, false, false, false, .light),
            ("settings-dark", .settings, false, false, false, false, .dark),
            ("search-dark", .search, false, false, false, false, .dark)
        ]
        for (name, tab, player, group, compact, end, scheme) in screens {
            let store = MockZoneStore()
            store.selectedTab = tab
            store.isPlayerPresented = player
            store.isRoomsSheetPresented = compact
            if group { store.beginGroupEditing(store.selectedZone) }
            try render(DeviceFrame(dark: scheme == .dark) {
                RemoteRoot().environmentObject(store).environment(\.sheetScrollToEnd, end)
            }.environment(\.colorScheme, scheme), named: name, in: output) {
                if name == "search-selected-room" { store.selectedZoneID = store.zone(for: "Movie Room").id }
            }
        }
        // The live Spotify library, with canned data: the mock store never reaches that branch of Browse.
        for (name, scheme) in [("spotify", ColorScheme.light), ("spotify-dark", ColorScheme.dark)] {
            let store = MockZoneStore()
            try render(DeviceFrame(dark: scheme == .dark) {
                ScrollView { SpotifyBrowse(model: .sample()).environmentObject(store) }.modifier(S1Surface())
            }.environment(\.colorScheme, scheme), named: name, in: output)
        }
        try contactSheet(in: output)
        print("Rendered \(screens.count + 2) screens at 780x1688 and overview.png at 3120x5064")
    }

    private static func render<Content: View>(_ content: Content, named name: String, in directory: URL, update: () -> Void = {}) throws {
        let frame = NSRect(x: 0, y: 0, width: 390, height: 844)
        let window = NSWindow(contentRect: frame, styleMask: .borderless, backing: .buffered, defer: false)
        let hosting = NSHostingView(rootView: content.environment(\.displayScale, 2).frame(width: 390, height: 844))
        window.contentView = hosting
        window.orderFront(nil)
        RunLoop.main.run(until: Date().addingTimeInterval(0.25))
        update()
        RunLoop.main.run(until: Date().addingTimeInterval(0.3))
        hideHostScrollbars(in: hosting)
        hosting.layoutSubtreeIfNeeded()
        RunLoop.main.run(until: Date().addingTimeInterval(0.1))
        // Render at a known 2x scale, independent of the attached Mac display.
        guard let bitmap = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: 780, pixelsHigh: 1688, bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0) else { throw RenderError.bitmap(name) }
        bitmap.size = NSSize(width: 390, height: 844)
        hosting.cacheDisplay(in: hosting.bounds, to: bitmap)
        guard let png = bitmap.representation(using: .png, properties: [:]) else { throw RenderError.bitmap(name) }
        let destination = directory.appendingPathComponent("\(name).png")
        try png.write(to: destination)
        window.orderOut(nil)
        print("\(destination.path) (780x1688)")
    }

    private static func hideHostScrollbars(in view: NSView) {
        // AppKit's Always preference overrides SwiftUI showsIndicators: false.
        // Respect the views' hidden indicators when hosting them on macOS.
        if let scroll = view as? NSScrollView {
            scroll.hasVerticalScroller = false
            scroll.hasHorizontalScroller = false
            scroll.scrollerStyle = .overlay
            scroll.tile()
        }
        view.subviews.forEach { hideHostScrollbars(in: $0) }
    }

    private static func contactSheet(in directory: URL) throws {
        guard let bitmap = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: 3120, pixelsHigh: 5064, bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0), let context = NSGraphicsContext(bitmapImageRep: bitmap) else { throw RenderError.bitmap("overview") }
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = context
        NSColor(white: 0.9, alpha: 1).setFill()
        NSRect(x: 0, y: 0, width: 3120, height: 5064).fill()
        for (index, name) in ["rooms", "rooms-dark", "browse", "now-playing", "group-rooms", "group-rooms-scrolled-end", "now-playing-rooms-sheet", "now-playing-rooms-sheet-scrolled-end", "settings", "search", "search-selected-room"].enumerated() {
            guard let image = NSImage(contentsOf: directory.appendingPathComponent("\(name).png")) else { throw RenderError.bitmap(name) }
            image.draw(in: NSRect(x: (index % 4) * 780, y: (2 - index / 4) * 1688, width: 780, height: 1688))
        }
        NSGraphicsContext.restoreGraphicsState()
        guard let png = bitmap.representation(using: .png, properties: [:]) else { throw RenderError.bitmap("overview") }
        let destination = directory.appendingPathComponent("overview.png")
        try png.write(to: destination)
        print(destination.path)
    }
    private enum RenderError: Error { case bitmap(String) }
}

// Only device furniture belongs to the renderer. All controller chrome is shared.
private struct DeviceFrame<Content: View>: View {
    let dark: Bool
    @ViewBuilder let content: () -> Content
    var body: some View {
        VStack(spacing: 0) {
            HStack {
                Text("9:41").font(.system(size: 15, weight: .semibold))
                Spacer()
                HStack(spacing: 5) {
                    Image(systemName: "cellularbars").font(.system(size: 13))
                    Image(systemName: "wifi").font(.system(size: 13, weight: .semibold))
                    Image(systemName: "battery.100percent").font(.system(size: 21))
                }
            }
            .padding(.horizontal, 24).padding(.top, 8).frame(height: 54)
            .foregroundStyle(dark ? .white : Color(white: 0.08))
            .background(dark ? Color(red: 0.04, green: 0.08, blue: 0.09) : .white)
            content().frame(maxWidth: .infinity, maxHeight: .infinity)
            Capsule().fill(.white.opacity(0.85)).frame(width: 134, height: 5)
                .frame(maxWidth: .infinity).frame(height: 34).background(Color(white: 0.025))
        }
        .frame(width: 390, height: 844)
    }
}
#endif
