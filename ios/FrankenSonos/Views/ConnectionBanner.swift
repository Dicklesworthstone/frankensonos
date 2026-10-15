import SwiftUI

struct ConnectionBanner: View {
    @EnvironmentObject private var store: MockZoneStore
    private var message: String {
        switch store.connectionStatus {
        case .offline: "Can't reach the daemon at \(store.daemonURLString)"
        case .reconnecting: "Reconnecting"
        case .refreshing: "Refreshing"
        case .live: store.streamNote ?? ""
        }
    }
    private var visible: Bool { store.connectionStatus != .live || store.streamNote != nil }
    var body: some View {
        if visible {
            HStack(spacing: 10) {
                Text(message).s1Font(12).fixedSize(horizontal: false, vertical: true)
                Spacer(minLength: 0)
                if store.connectionStatus == .offline {
                    Button("Retry", action: store.retry).s1Font(12, weight: .semibold)
                } else if store.connectionStatus != .live { ProgressView().controlSize(.small) }
            }
            .padding(12).foregroundStyle(.primary)
            .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 8))
            .padding(12).accessibilityElement(children: .contain)
        }
    }
}
