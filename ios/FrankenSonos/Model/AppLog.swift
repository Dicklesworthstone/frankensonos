import Foundation

/// A short in-memory log. Settings copies it to the clipboard so a problem on the phone can be pasted into a message
/// without a Mac and a cable. It keeps the newest `limit` lines and never records anything but connection facts: URLs,
/// status codes, the daemon's own error text and counts. No account data passes through this app.
final class AppLog: @unchecked Sendable {
    static let shared = AppLog()
    static let limit = 600

    private let lock = NSLock()
    private var lines: [String] = []

    func add(_ category: String, _ message: String) {
        let stamp = Self.stamp(Date())
        lock.lock()
        lines.append("\(stamp) [\(category)] \(message)")
        if lines.count > Self.limit { lines.removeFirst(lines.count - Self.limit) }
        lock.unlock()
    }

    /// The text that goes on the clipboard: a title, the header lines the caller supplies, then the log.
    func report(header: [String]) -> String {
        lock.lock()
        let copy = lines
        lock.unlock()
        return (["FrankenSonos log"] + header + [""] + copy).joined(separator: "\n")
    }

    var count: Int {
        lock.lock()
        defer { lock.unlock() }
        return lines.count
    }

    private static func stamp(_ date: Date) -> String {
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        return formatter.string(from: date)
    }
}
