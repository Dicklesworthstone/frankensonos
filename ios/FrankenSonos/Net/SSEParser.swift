import Foundation

struct DaemonEvent {
    let id: String?
    let kind: String
    let data: String
}

struct SSEParser {
    private var id: String?
    private var kind = "message"
    private var data: [String] = []

    mutating func consume(_ raw: String) -> DaemonEvent? {
        let line = raw.hasSuffix("\r") ? String(raw.dropLast()) : raw
        if line.isEmpty {
            defer { kind = "message"; data = [] }
            guard !data.isEmpty else { return nil }
            return DaemonEvent(id: id, kind: kind, data: data.joined(separator: "\n"))
        }
        guard !line.hasPrefix(":") else { return nil }
        let pair = line.split(separator: ":", maxSplits: 1, omittingEmptySubsequences: false)
        var value = pair.count == 2 ? String(pair[1]) : ""
        if value.hasPrefix(" ") { value.removeFirst() }
        switch pair[0] {
        case "id": if !value.contains("\0") { id = value }
        case "event": kind = value.isEmpty ? "message" : value
        case "data": data.append(value)
        default: break
        }
        return nil
    }
}
