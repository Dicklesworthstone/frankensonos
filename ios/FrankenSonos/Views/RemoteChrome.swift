import SwiftUI

struct ScaledS1Font: ViewModifier {
    @ScaledMetric private var size: CGFloat
    let weight: Font.Weight
    init(_ size: CGFloat, weight: Font.Weight) { _size = ScaledMetric(wrappedValue: size); self.weight = weight }
    func body(content: Content) -> some View { content.font(.system(size: size, weight: weight)) }
}
extension View {
    func s1Font(_ size: CGFloat, weight: Font.Weight = .regular) -> some View { modifier(ScaledS1Font(size, weight: weight)) }
}

enum S1Palette {
    static func secondary(_ scheme: ColorScheme) -> Color {
        scheme == .dark ? Color(red: 0.72, green: 0.72, blue: 0.72) : Color(red: 0.38, green: 0.38, blue: 0.38)
    }
    /// A grouped panel on a page (Settings): white on light gray, dark gray on near black.
    static func panel(_ scheme: ColorScheme) -> Color { scheme == .dark ? Color(white: 0.14) : .white }
    static func page(_ scheme: ColorScheme) -> Color { scheme == .dark ? Color(white: 0.075) : Color(white: 0.95) }
    /// A text field or an unselected chip sitting on the surface.
    static func field(_ scheme: ColorScheme) -> Color { scheme == .dark ? Color(white: 0.17) : Color(white: 0.94) }
    static func chipSelected(_ scheme: ColorScheme) -> Color { scheme == .dark ? .white : Color(white: 0.15) }
    static func chipSelectedText(_ scheme: ColorScheme) -> Color { scheme == .dark ? .black : .white }
    static func equalizer(_ scheme: ColorScheme) -> Color {
        scheme == .dark ? Color(red: 0.88, green: 0.88, blue: 0.88) : Color(red: 0.2, green: 0.2, blue: 0.2)
    }
}

struct S1Surface: ViewModifier {
    @Environment(\.colorScheme) private var scheme
    func body(content: Content) -> some View { content.background(scheme == .dark ? Color(white: 0.075) : .white) }
}

struct EqualizerGlyph: View {
    @Environment(\.colorScheme) private var scheme
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    var body: some View {
        TimelineView(.animation(minimumInterval: 0.25, paused: reduceMotion)) { timeline in
            let phase = timeline.date.timeIntervalSinceReferenceDate
            HStack(alignment: .bottom, spacing: 2) {
                ForEach(0..<3) { index in
                    Capsule().frame(width: 2, height: reduceMotion ? CGFloat([6, 12, 9][index]) : 4 + CGFloat((sin(phase * 4 + Double(index) * 2) + 1) * 5))
                }
            }
            .frame(width: 10, height: 16, alignment: .bottom)
        }
        .foregroundStyle(S1Palette.equalizer(scheme))
        .accessibilityLabel("Playing")
    }
}

struct ThinSlider: View {
    @Binding var value: Double
    var label: String
    var thumbSize: CGFloat = 10
    var onEditingChanged: (Bool) -> Void = { _ in }
    @State private var isDragging = false
    var body: some View {
        GeometryReader { geometry in
            let width = max(1, geometry.size.width - thumbSize)
            let level = min(1, max(0, value))
            ZStack(alignment: .leading) {
                Capsule().fill(Color.primary.opacity(0.17)).frame(height: 2)
                Capsule().fill(Color.primary.opacity(0.85)).frame(width: width * level + thumbSize / 2, height: 2)
                Circle().fill(Color.primary).frame(width: thumbSize, height: thumbSize).offset(x: width * level)
            }
            .frame(maxHeight: .infinity)
            .contentShape(Rectangle())
            .gesture(DragGesture(minimumDistance: 0).onChanged {
                if !isDragging { isDragging = true; onEditingChanged(true) }
                value = min(1, max(0, ($0.location.x - thumbSize / 2) / width))
            }.onEnded { _ in isDragging = false; onEditingChanged(false) })
        }
        .frame(height: 16)
        .accessibilityElement()
        .accessibilityLabel(label)
        .accessibilityValue("\(Int(value * 100)) percent")
        .accessibilityAdjustableAction { direction in
            onEditingChanged(true)
            if direction == .increment { value = min(1, value + 0.05) }
            if direction == .decrement { value = max(0, value - 0.05) }
            onEditingChanged(false)
        }
    }
}

struct S1TopBar: View {
    let title: String
    var leading: String? = nil
    var trailing: String? = nil
    var leadingAction: () -> Void = {}
    var trailingAction: (() -> Void)? = nil
    var trailingDisabled = false
    var body: some View {
        ZStack {
            Text(title).s1Font(17, weight: .semibold).lineLimit(1).padding(.horizontal, 80)
            HStack {
                if let leading { Button(leading, action: leadingAction).s1Font(13).frame(minHeight: 44) }
                Spacer()
                if let trailing {
                    if let trailingAction {
                        Button(trailing, action: trailingAction).s1Font(13).frame(minHeight: 44).disabled(trailingDisabled)
                    } else {
                        Text(trailing).s1Font(13)
                    }
                }
            }
            .padding(.horizontal, 16)
        }
        .frame(minHeight: 48)
        .overlay(alignment: .bottom) { Rectangle().fill(Color.primary.opacity(0.09)).frame(height: 0.5) }
        .buttonStyle(.plain)
    }
}

struct RoomSwitcherStrip: View {
    @EnvironmentObject private var store: MockZoneStore
    @ScaledMetric private var chipHeight = 32.0
    @ScaledMetric private var stripHeight = 44.0
    var body: some View {
        ScrollViewReader { proxy in
            ScrollView(.horizontal, showsIndicators: false) {
                HStack(spacing: 8) {
                    ForEach(store.zones) { zone in
                        Button { store.selectedZoneID = zone.id } label: {
                            HStack(spacing: 6) {
                                AlbumArtworkView(track: zone.track).frame(width: 24, height: 24)
                                Text(zone.shortName).s1Font(12, weight: zone.id == store.selectedZoneID ? .semibold : .regular).lineLimit(1).fixedSize(horizontal: true, vertical: false)
                                if zone.roomNames.count > 1 { Image(systemName: "link").font(.system(size: 10)) }
                                if zone.isPlaying { EqualizerGlyph().scaleEffect(0.7).frame(width: 9) }
                                else { Circle().fill(Color.secondary).frame(width: 4, height: 4).accessibilityLabel("Paused") }
                            }
                            .padding(.horizontal, 8)
                            .frame(height: chipHeight)
                            .background(Color.primary.opacity(zone.id == store.selectedZoneID ? 0.07 : 0.025), in: RoundedRectangle(cornerRadius: 6))
                            .overlay { RoundedRectangle(cornerRadius: 6).stroke(Color.primary.opacity(zone.id == store.selectedZoneID ? 0.75 : 0.08), lineWidth: 0.75) }
                            .contentShape(Rectangle())
                        }
                        .id(zone.id)
                        .buttonStyle(.plain)
                        .onLongPressGesture { store.beginGroupEditing(zone) }
                        .accessibilityLabel("\(zone.displayName), \(zone.isPlaying ? "playing" : "paused")")
                        .accessibilityAddTraits(zone.id == store.selectedZoneID ? .isSelected : [])
                        .accessibilityAction(named: "Group rooms") { store.beginGroupEditing(zone) }
                    }
                }
                .padding(.trailing, 32)
            }
            .onAppear { proxy.scrollTo(store.selectedZoneID, anchor: .leading) }
            .onChange(of: store.selectedZoneID) { _, id in
                DispatchQueue.main.async {
                    proxy.scrollTo(id, anchor: .center)
                }
            }
        }
        .clipped()
        .padding(.leading, 16)
        .mask {
            HStack(spacing: 0) {
                Color.white
                LinearGradient(colors: [.white, .clear], startPoint: .leading, endPoint: .trailing).frame(width: 32)
            }
        }
        .frame(height: stripHeight)
        .overlay(alignment: .bottom) { Rectangle().fill(Color.primary.opacity(0.08)).frame(height: 0.5) }
    }
}

struct S1BottomTabBar: View {
    @EnvironmentObject private var store: MockZoneStore
    var body: some View {
        HStack(spacing: 0) {
            tab("Music", "music.note", .browse)
            tab("Rooms", "house", .rooms)
            tab("Search", "magnifyingglass", .search)
            tab("Settings", "gearshape", .settings)
        }
        .padding(.top, 5)
        .frame(minHeight: 52)
        .background(Color(white: 0.025))
    }
    private func tab(_ name: String, _ symbol: String, _ value: RemoteTab) -> some View {
        Button {
            store.selectedTab = value
            store.isPlayerPresented = false
        } label: {
            VStack(spacing: 4) {
                Image(systemName: symbol).font(.system(size: 20, weight: .regular))
                Text(name).s1Font(9, weight: .medium)
            }
            .foregroundStyle(store.selectedTab == value ? .white : Color(white: 0.56))
            .frame(maxWidth: .infinity, minHeight: 44)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityAddTraits(store.selectedTab == value ? .isSelected : [])
    }
}

struct RemoteShell: View {
    @EnvironmentObject private var store: MockZoneStore
    private var title: String {
        switch store.selectedTab {
        case .browse: store.browseSource?.name ?? "Music"
        case .rooms: "Rooms"
        case .search: "Search"
        case .settings: "Settings"
        }
    }
    var body: some View {
        VStack(spacing: 0) {
            S1TopBar(title: title, leading: store.selectedTab == .rooms ? "Group" : (store.browseSource != nil && store.selectedTab == .browse ? "Back" : nil), trailing: store.selectedTab == .rooms ? "Pause All" : nil, leadingAction: {
                if store.selectedTab == .rooms { store.beginGroupEditing(store.selectedZone) }
                else { store.browseSource = nil }
            }, trailingAction: store.pauseAll)
            if store.selectedTab != .rooms && store.selectedTab != .settings { RoomSwitcherStrip() }
            Group {
                switch store.selectedTab {
                case .rooms: RoomsView()
                case .browse: BrowseView(openPlayer: showPlayer)
                case .search: SearchView(openPlayer: showPlayer)
                case .settings: SettingsView()
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
            MiniPlayerBar(openPlayer: showPlayer)
            S1BottomTabBar()
        }
        .modifier(S1Surface())
    }
    private func showPlayer() { store.isPlayerPresented = true }
}

struct RemoteRoot: View {
    @EnvironmentObject private var store: MockZoneStore
    #if os(iOS)
    @State private var roomsDetent: PresentationDetent = .medium
    #endif
    var body: some View {
        ZStack(alignment: .bottom) {
            if store.isPlayerPresented { NowPlayingView() }
            else { RemoteShell() }
            #if os(macOS)
            if store.isRoomsSheetPresented {
                sheetBackdrop { store.isRoomsSheetPresented = false }
                CompactRoomsSheet().frame(height: 400).transition(.move(edge: .bottom))
            }
            #endif
            if store.groupZone != nil {
                sheetBackdrop { store.groupZone = nil }
                GroupRoomsSheet().transition(.move(edge: .bottom))
            }
            if store.isQueuePresented || store.isSleepPresented {
                sheetBackdrop { store.isQueuePresented = false; store.isSleepPresented = false }
                PlayerUtilitySheet()
            }
        }
        .animation(.easeInOut(duration: 0.22), value: store.isRoomsSheetPresented)
        .animation(.easeInOut(duration: 0.22), value: store.groupZone != nil)
        #if os(iOS)
        .sheet(isPresented: $store.isRoomsSheetPresented, onDismiss: { roomsDetent = .medium }) {
            CompactRoomsSheet().environmentObject(store)
                .presentationDetents([.medium, .large], selection: $roomsDetent)
                .presentationDragIndicator(.visible)
                .presentationCornerRadius(16)
                .presentationBackground(store.isPlayerPresented ? Color(white: 0.075) : .white)
                .preferredColorScheme(store.isPlayerPresented ? .dark : .light)
        }
        #endif
        .safeAreaInset(edge: .top, spacing: 0) {
            if store.isLive { ConnectionBanner() }
        }
        .alert("Didn't go through", isPresented: Binding(get: { store.isLive && store.commandError != nil }, set: { if !$0 { store.commandError = nil } })) {
            ForEach(store.commandSuggestions, id: \.self) { suggestion in
                Button(suggestion) {
                    if let zone = store.zones.first(where: { $0.roomNames.contains(suggestion) }) { store.selectedZoneID = zone.id }
                    store.commandError = nil
                }
            }
            Button("OK") { store.commandError = nil }
        } message: { Text(store.commandError ?? "") }
        .scrollIndicators(.hidden)
        .onReceive(Timer.publish(every: 1, on: .main, in: .common).autoconnect()) { store.tick(now: $0) }
        .safeAreaInset(edge: .bottom, spacing: 0) { Color(white: 0.025).frame(height: 0) }
    }
    private func sheetBackdrop(_ close: @escaping () -> Void) -> some View {
        Color.black.opacity(0.48).ignoresSafeArea().onTapGesture(perform: close).accessibilityLabel("Dismiss sheet").accessibilityAddTraits(.isButton)
    }
}
