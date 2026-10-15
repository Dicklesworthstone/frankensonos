import SwiftUI

#if canImport(UIKit)
import UIKit

enum SystemColors {
    static let background = Color(uiColor: .systemBackground)
    static let secondaryBackground = Color(uiColor: .secondarySystemBackground)
    static let separator = Color(uiColor: .separator)
}
#else
import AppKit

enum SystemColors {
    static let background = Color(nsColor: .windowBackgroundColor)
    static let secondaryBackground = Color(nsColor: .controlBackgroundColor)
    static let separator = Color(nsColor: .separatorColor)
}
#endif
