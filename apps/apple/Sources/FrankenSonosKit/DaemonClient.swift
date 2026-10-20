import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif

public struct DaemonEndpoint: Hashable, Sendable {
    public let url: URL
    public init(_ input: String, allowLocalHTTP: Bool = false) throws {
        guard var c = URLComponents(string: input.trimmingCharacters(in: .whitespacesAndNewlines)),
              let host = c.host, !host.isEmpty, c.user == nil, c.password == nil,
              c.query == nil, c.fragment == nil, c.path.isEmpty || c.path == "/",
              c.port == nil || (1...65535).contains(c.port!) else {
            throw ContractError.invalid("Enter the daemon origin, without a path, credentials, query, or fragment.")
        }
        let scheme = c.scheme?.lowercased()
        guard scheme == "https" || (scheme == "http" && allowLocalHTTP && Self.isLocal(host)) else {
            throw ContractError.invalid("Use HTTPS for your tailnet. Local HTTP must be explicitly enabled and name a local host.")
        }
        c.scheme = scheme; c.path = ""; c.host = host.lowercased()
        guard let url = c.url else { throw ContractError.invalid("The daemon address is invalid.") }
        self.url = url
    }
    private static func isLocal(_ value: String) -> Bool {
        let host = value.lowercased().trimmingCharacters(in: CharacterSet(charactersIn: "[]"))
        if host == "localhost" || host == "::1" || host.hasSuffix(".local") { return true }
        let parts = host.split(separator: ".", omittingEmptySubsequences: false)
        if parts.count == 4, let a = UInt8(parts[0]), let b = UInt8(parts[1]),
           UInt8(parts[2]) != nil, UInt8(parts[3]) != nil {
            return a == 127 || a == 10 || (a == 172 && (16...31).contains(b)) || (a == 192 && b == 168)
        }
        return !host.contains(".") && !host.contains(":") &&
            host.allSatisfy { $0.isASCII && ($0.isLetter || $0.isNumber || $0 == "-") } &&
            !host.allSatisfy(\.isNumber)
    }
    /// Every dynamic path element is encoded independently, including slash and @.
    public func resource(_ segments: [String], query: [URLQueryItem] = []) throws -> URL {
        guard !segments.contains(where: { $0.isEmpty || $0 == "." || $0 == ".." }) else {
            throw ContractError.invalid("The resource path is invalid.")
        }
        var c = URLComponents(url: url, resolvingAgainstBaseURL: false)!
        let allowed = CharacterSet.alphanumerics.union(CharacterSet(charactersIn: "-._~"))
        c.percentEncodedPath = "/" + segments.map { $0.addingPercentEncoding(withAllowedCharacters: allowed)! }.joined(separator: "/")
        c.queryItems = query.isEmpty ? nil : query
        // The daemon decodes query values as form data: a literal plus must
        // be escaped, not interpreted as a space in a room or client name.
        c.percentEncodedQuery = c.percentEncodedQuery?.replacingOccurrences(of: "+", with: "%2B")
        guard let result = c.url else { throw ContractError.invalid("The resource address is invalid.") }
        return result
    }
}

public struct APIProblem: Codable, Error, LocalizedError, Equatable, Sendable {
    public let code: String
    public let detail: String
    public let hint: String?
    public var errorDescription: String? { [detail, hint].compactMap { $0 }.joined(separator: "\n") }
}
public enum ClientError: Error, LocalizedError, Sendable, Equatable {
    case http(Int, String)
    case unreadable
    case uncertain(String)
    case eventOverflow
    public var errorDescription: String? {
        switch self {
        case .http(let status, let detail): "Daemon response \(status): \(detail)"
        case .unreadable: "The daemon returned an incomplete response. Refresh before acting."
        case .uncertain(let detail): "The action may have taken effect. It was not repeated. Refresh the affected rooms before another command.\n\(detail)"
        case .eventOverflow: "Live updates fell behind. A fresh house snapshot is required."
        }
    }
}
public struct HTTPReply: Sendable {
    public let data: Data
    public let status: Int
    public init(data: Data, status: Int) { self.data = data; self.status = status }
}
public protocol DaemonTransport: Sendable {
    func send(_ request: URLRequest) async throws -> HTTPReply
    func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error>
}

private final class NoRedirects: NSObject, URLSessionTaskDelegate, Sendable {
    func urlSession(_ session: URLSession, task: URLSessionTask, willPerformHTTPRedirection response: HTTPURLResponse,
                    newRequest request: URLRequest, completionHandler: @escaping @Sendable (URLRequest?) -> Void) {
        completionHandler(nil)
    }
}
public struct URLSessionTransport: DaemonTransport {
    static let maxReplyBytes = 4 * 1_024 * 1_024
    private let session: URLSession

    static func checkedHTTP(_ response: URLResponse) throws -> HTTPURLResponse {
        guard let http = response as? HTTPURLResponse else { throw ClientError.unreadable }
        let announced = http.expectedContentLength
        if announced > Int64(maxReplyBytes) { throw ClientError.unreadable }
        return http
    }

    static func append(_ byte: UInt8, to data: inout Data) throws {
        guard data.count < maxReplyBytes else { throw ClientError.unreadable }
        data.append(byte)
    }

    public init() {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.httpCookieStorage = nil
        configuration.urlCredentialStorage = nil
        configuration.urlCache = nil
        configuration.timeoutIntervalForResource = 3_600
        session = URLSession(configuration: configuration, delegate: NoRedirects(), delegateQueue: nil)
    }
    public func send(_ request: URLRequest) async throws -> HTTPReply {
        #if canImport(Darwin)
        // Apple platforms stream ordinary API replies too: reject an announced
        // oversize before buffering and stop at the hard cap even for chunked
        // or dishonest responses.
        let (bytes, response) = try await session.bytes(for: request)
        let http = try Self.checkedHTTP(response)
        var data = Data()
        if http.expectedContentLength > 0 {
            data.reserveCapacity(min(Int(http.expectedContentLength), Self.maxReplyBytes))
        }
        for try await byte in bytes {
            try Task.checkCancellation()
            try Self.append(byte, to: &data)
        }
        return HTTPReply(data: data, status: http.statusCode)
        #else
        // FoundationNetworking does not expose URLSession.AsyncBytes. Keep the
        // same contract for package tests and non-Apple tools, with a post-read
        // hard cap; shipping Apple clients use the streaming branch above.
        let (data, response) = try await session.data(for: request)
        let http = try Self.checkedHTTP(response)
        guard data.count <= Self.maxReplyBytes else { throw ClientError.unreadable }
        return HTTPReply(data: data, status: http.statusCode)
        #endif
    }
    public func events(_ request: URLRequest) -> AsyncThrowingStream<EventSignal, Error> {
        AsyncThrowingStream(bufferingPolicy: .bufferingNewest(128)) { continuation in
            let task = Task {
                do {
                    #if canImport(Darwin)
                    let (bytes, response) = try await session.bytes(for: request)
                    guard let http = response as? HTTPURLResponse, http.statusCode == 200,
                          http.value(forHTTPHeaderField: "Content-Type")?.lowercased().hasPrefix("text/event-stream") == true else {
                        throw ClientError.unreadable
                    }
                    continuation.yield(.connected)
                    var parser = EventParser()
                    for try await byte in bytes {
                        try Task.checkCancellation()
                        if let event = try parser.feed(byte) {
                            if case .dropped = continuation.yield(.event(event)) { throw ClientError.eventOverflow }
                        }
                    }
                    continuation.finish()
                    #else
                    // The native stream is compiled on Apple SDKs. Linux exercises its parser and injected transport.
                    throw ContractError.invalid("URLSession streaming requires an Apple platform.")
                    #endif
                } catch { continuation.finish(throwing: error) }
            }
            continuation.onTermination = { @Sendable _ in task.cancel() }
        }
    }
}

public struct DaemonClient: Sendable {
    public let endpoint: DaemonEndpoint
    private let transport: any DaemonTransport
    public init(endpoint: DaemonEndpoint, transport: any DaemonTransport = URLSessionTransport()) {
        self.endpoint = endpoint; self.transport = transport
    }
    public static func decode<T: Decodable>(_ type: T.Type, from data: Data) throws -> T {
        let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
        return try decoder.decode(type, from: data)
    }
    public func get<T: Decodable & Sendable>(_ type: T.Type, _ path: [String], query: [URLQueryItem] = []) async throws -> T {
        var request = URLRequest(url: try endpoint.resource(path, query: query), cachePolicy: .reloadIgnoringLocalCacheData, timeoutInterval: 15)
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        let reply = try await transport.send(request)
        try Self.check(reply)
        return try Self.decode(type, from: reply.data)
    }
    public func zones() async throws -> [Zone] { try Zone.validate(await get([Zone].self, ["zones"])) }
    public func state(_ zone: Zone, room: String? = nil) async throws -> ZoneState {
        let address = RoomID(household: zone.household, room: room ?? zone.coordinatorRoom).address
        return try await get(ZoneState.self, ["zones", address, "state"]).validated(for: zone)
    }
    public func queue(_ zone: Zone, offset: UInt32 = 0) async throws -> QueuePage {
        try await get(QueuePage.self, ["zones", zone.id.address, "queue"], query: [
            URLQueryItem(name: "offset", value: String(offset)), URLQueryItem(name: "limit", value: "25")
        ]).validated(offset: offset)
    }
    public func dj(_ zone: Zone) async throws -> DJStatus {
        try await get(DJStatus.self, ["zones", zone.id.address, "dj"]).validated(for: zone)
    }
    public func favorites(_ zone: Zone) async throws -> [Favorite] {
        try await get([Favorite].self, ["favorites"], query: [URLQueryItem(name: "zone", value: zone.id.address)])
    }
    public func search(_ query: String, in zone: Zone) async throws -> [LibraryHit] {
        try await get([LibraryHit].self, ["library", "search"], query: [
            URLQueryItem(name: "zone", value: zone.id.address), URLQueryItem(name: "q", value: query),
            URLQueryItem(name: "limit", value: "25")
        ])
    }
    /// Artwork always goes through the configured daemon. Track metadata only
    /// versions its cache key; a speaker or service URL is never loaded by UI.
    public func artworkURL(in zone: Zone, track: Track?) -> URL? {
        guard let track, !track.uri.isEmpty, track.uri != "NOT_IMPLEMENTED",
              (try? zone.validated()) != nil else { return nil }
        // A small stable cache tag stays within /art's 64-character contract.
        // Include radio metadata because the URI can stay fixed between songs.
        var tag: UInt64 = 14_695_981_039_346_656_037
        for value in [track.uri, track.title ?? "", track.creator ?? "", track.album ?? "",
                      track.queuePosition.map(String.init) ?? ""] {
            for byte in "\(value.utf8.count):\(value)".utf8 {
                tag = (tag ^ UInt64(byte)) &* 1_099_511_628_211
            }
        }
        return try? endpoint.resource(["art"], query: [
            URLQueryItem(name: "zone", value: zone.id.address),
            URLQueryItem(name: "v", value: String(tag, radix: 16))
        ])
    }
    public func eventStream(after cursor: String?) throws -> AsyncThrowingStream<EventSignal, Error> {
        var request = URLRequest(url: try endpoint.resource(["events"]), cachePolicy: .reloadIgnoringLocalCacheData, timeoutInterval: 45)
        request.setValue("text/event-stream", forHTTPHeaderField: "Accept")
        if let cursor, UInt64(cursor) != nil { request.setValue(cursor, forHTTPHeaderField: "Last-Event-ID") }
        return transport.events(request)
    }
    /// No command is retried, redirected, queued offline, or replayed after reconnect.
    public func perform(_ command: NativeCommand, in zone: Zone) async throws -> CommandReceipt {
        try Task.checkCancellation()
        let payload = try command.payload(in: zone)
        // Grouping/move/party depend on current topology. Re-read immediately
        // before the write; failures here are definite because nothing has
        // been sent yet. The daemon remains authoritative for the final race.
        if command.requiresFreshTopology {
            try command.validateTopology(reviewed: zone, current: try await zones())
        }
        var request = URLRequest(url: try endpoint.resource(payload.path), cachePolicy: .reloadIgnoringLocalCacheData, timeoutInterval: payload.timeout)
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.httpBody = try JSONEncoder().encode(payload.body)
        try Task.checkCancellation()
        do {
            let reply = try await transport.send(request)
            try Self.check(reply)
            return try command.receipt(reply.data, in: zone)
        } catch {
            // Once handed to URLSession, even cancellation is not evidence of no effect.
            throw ClientError.uncertain(error.localizedDescription)
        }
    }
    private static func check(_ reply: HTTPReply) throws {
        guard (200..<300).contains(reply.status) else {
            if let problem = try? decode(APIProblem.self, from: reply.data) { throw problem }
            throw ClientError.http(reply.status, "Check the configured daemon and its access policy.")
        }
    }
}


extension DaemonClient {
    /// Dispatch only the immutable, one-use scene review. A response lost after
    /// the PUT/DELETE was handed to transport is uncertain and is never retried.
    func perform(_ review: SceneMutationReview) async throws -> SceneMutationReceipt {
        try Task.checkCancellation()
        var request = URLRequest(url: try endpoint.resource(["scenes", review.name]),
                                 cachePolicy: .reloadIgnoringLocalCacheData, timeoutInterval: 120)
        request.httpMethod = review.command.isDelete ? "DELETE" : "PUT"
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        if !review.command.isDelete {
            request.setValue("application/json", forHTTPHeaderField: "Content-Type")
            if let definition = review.observation.definition {
                request.httpBody = try JSONEncoder().encode(definition)
            } else {
                request.httpBody = Data("{}".utf8)
            }
        }
        try Task.checkCancellation()
        do {
            let reply = try await transport.send(request)
            try Self.check(reply)
            return try review.receipt(reply.data)
        } catch {
            throw ClientError.uncertain(error.localizedDescription)
        }
    }
}

extension DaemonClient {
    /// Only the shared one-use review session dispatches recovery. The caller,
    /// saved state, force policy and affected rooms are never client supplied.
    func perform(_ review: UndoReview) async throws -> UndoReceipt {
        try Task.checkCancellation()
        try review.action.validateUndoCandidate()
        var request = URLRequest(url: try endpoint.resource(["undo", "checked"]),
                                 cachePolicy: .reloadIgnoringLocalCacheData, timeoutInterval: 120)
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.httpBody = try JSONEncoder().encode(["expected_id": review.action.id])
        try Task.checkCancellation()
        do {
            let reply = try await transport.send(request)
            try Self.check(reply)
            return try Self.decode(UndoReceipt.self, from: reply.data).validated(for: review.action)
        } catch {
            // A lost or malformed response is not proof that restoration did
            // not happen. Keep the original action attribution and never retry.
            throw ClientError.uncertain(error.localizedDescription)
        }
    }
}

extension DaemonClient {
    /// The low-level schedule write, like perform(NativeCommand). UI callers
    /// use CommandReviewSession for one-use review and fresh preflight. Once
    /// sent, even a failed receipt can mean a schedule was stored: never retry.
    public func perform(_ command: ScheduleCommand) async throws -> ScheduleReceipt {
        try Task.checkCancellation()
        let payload = try command.payload()
        var request = URLRequest(url: try endpoint.resource(payload.path), cachePolicy: .reloadIgnoringLocalCacheData, timeoutInterval: 120)
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.httpBody = try JSONEncoder().encode(payload.body)
        try Task.checkCancellation()
        do {
            let reply = try await transport.send(request)
            try Self.check(reply)
            return try command.receipt(reply.data)
        } catch {
            throw ClientError.uncertain(error.localizedDescription)
        }
    }
}

extension DaemonClient {
    /// These operations affect daemon sign-in/cache state, never speaker
    /// playback. No redirects, retries or credentials are added by the client.
    public func beginSpotifySignIn() async throws -> SpotifyConsent {
        try await spotifyPost(SpotifyConsent.self, path: ["auth", "spotify", "begin"], body: [:]) {
            _ = try $0.authorizationURL(); return $0
        }
    }

    public func completeSpotifySignIn(_ callback: String, consent: SpotifyConsent) async throws -> SpotifySignIn {
        let redirect = try consent.callback(callback)
        return try await spotifyPost(SpotifySignIn.self, path: ["auth", "spotify", "complete"], body: ["redirect": redirect]) {
            let result = try $0.validated()
            guard result.signedIn, !result.pending, result.redirectUri == consent.redirectUri else { throw ClientError.unreadable }
            return result
        }
    }

    public func startSpotifyLibrarySync() async throws -> SpotifyLibrarySync {
        try await spotifyPost(SpotifyLibrarySync.self, path: ["dj", "sync"], body: [:]) { try $0.validated() }
    }

    private func spotifyPost<T: Decodable & Sendable>(_ type: T.Type, path: [String], body: [String: String],
                                                     validate: (T) throws -> T) async throws -> T {
        try Task.checkCancellation()
        var request = URLRequest(url: try endpoint.resource(path), cachePolicy: .reloadIgnoringLocalCacheData, timeoutInterval: 120)
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.httpBody = try JSONEncoder().encode(body)
        try Task.checkCancellation()
        do {
            let reply = try await transport.send(request)
            try Self.check(reply)
            return try validate(Self.decode(type, from: reply.data))
        } catch {
            // A transport/API error may echo the callback address, code or
            // consent state. Never publish its text, and never repeat the POST.
            throw SpotifySetupError.uncertain
        }
    }
}
