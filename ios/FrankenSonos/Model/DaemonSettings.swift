import Foundation

enum DaemonSettings {
    private static let key = "daemonURL"
    /// False until a daemon URL has been chosen, by typing it or by picking one found with Bonjour.
    static var isConfigured: Bool { UserDefaults.standard.string(forKey: key) != nil }
    static var urlString: String {
        get { UserDefaults.standard.string(forKey: key) ?? "http://127.0.0.1:8099" }
        set { UserDefaults.standard.set(newValue, forKey: key) }
    }
    static var url: URL {
        URL(string: urlString) ?? URL(string: "http://127.0.0.1:8099")!
    }
    static func validated(_ text: String) -> URL? {
        guard let url = URL(string: text.trimmingCharacters(in: .whitespacesAndNewlines)),
              ["http", "https"].contains(url.scheme?.lowercased() ?? ""),
              url.host != nil, url.user == nil, url.password == nil,
              url.query == nil, url.fragment == nil else { return nil }
        return url
    }
}
