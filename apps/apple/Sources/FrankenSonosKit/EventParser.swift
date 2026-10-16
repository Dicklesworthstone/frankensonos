import Foundation

public struct ServerEvent: Equatable, Sendable {
    public let id: String?
    public let kind: String
    public let data: String
}
public enum EventSignal: Sendable {
    case connected
    case event(ServerEvent)
}

/// Incremental SSE framing. Heartbeats are ignored; incomplete final events are discarded.
public struct EventParser: Sendable {
    private var line: [UInt8] = []
    private var data: [String] = []
    private var kind = ""
    private var id: String?
    private var size = 0
    private var previousCR = false
    private var firstLine = true
    public init() {}
    public mutating func feed(_ byte: UInt8) throws -> ServerEvent? {
        if byte == 10 && previousCR { previousCR = false; return nil }
        previousCR = byte == 13
        if byte == 10 || byte == 13 { return try endLine() }
        guard line.count < 65_536 else { throw ClientError.eventOverflow }
        line.append(byte)
        return nil
    }
    private mutating func endLine() throws -> ServerEvent? {
        guard var text = String(bytes: line, encoding: .utf8) else { throw ClientError.unreadable }
        line.removeAll(keepingCapacity: true)
        if firstLine { firstLine = false; if text.hasPrefix("\u{feff}") { text.removeFirst() } }
        if text.isEmpty {
            let event = data.isEmpty ? nil : ServerEvent(id: id, kind: kind.isEmpty ? "message" : kind, data: data.joined(separator: "\n"))
            data.removeAll(keepingCapacity: true); kind = ""; size = 0
            return event
        }
        if text.hasPrefix(":") { return nil }
        let pair = text.split(separator: ":", maxSplits: 1, omittingEmptySubsequences: false)
        let field = String(pair[0])
        var value = pair.count == 2 ? String(pair[1]) : ""
        if value.hasPrefix(" ") { value.removeFirst() }
        size += text.utf8.count
        guard size <= 131_072 else { throw ClientError.eventOverflow }
        switch field {
        case "event": kind = value
        case "data": data.append(value)
        case "id": if !value.contains("\0") { id = value.isEmpty ? nil : value }
        default: break
        }
        return nil
    }
}
