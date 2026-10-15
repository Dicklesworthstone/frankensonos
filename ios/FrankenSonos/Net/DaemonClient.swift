import Foundation

@MainActor
final class DaemonClient {
    let baseURL: URL
    private let session: URLSession
    static let backoff: [Double] = [1, 2, 4, 8, 15]

    init(baseURL: URL) {
        self.baseURL = baseURL
        let configuration = URLSessionConfiguration.ephemeral
        configuration.timeoutIntervalForRequest = 10
        configuration.timeoutIntervalForResource = 15
        session = URLSession(configuration: configuration)
    }

    func zones() async throws -> [ZoneDTO] { try await get(url("zones")) }
    func rooms() async throws -> [RoomDTO] { try await get(url("rooms")) }
    func state(room: String) async throws -> ZoneStateDTO { try await get(stateURL(room: room)) }
    func health() async throws { _ = try await data(URLRequest(url: url("health"))) }
    func favorites(room: String) async throws -> [FavoriteDTO] {
        var components = URLComponents(url: url("favorites"), resolvingAgainstBaseURL: false)!
        components.queryItems = [URLQueryItem(name: "zone", value: room)]
        components.percentEncodedQuery = components.percentEncodedQuery?.replacingOccurrences(of: "+", with: "%2B")
        return try await get(components.url!)
    }

    func spotifyStatus() async throws -> SpotifyStatus { try await get(url("spotify/status")) }
    func spotifySync() async throws { try await command("dj/sync", body: [:]) }
    /// Finish a sign-in the app started: the daemon trades the code for tokens and keeps them.
    func spotifyExchange(code: String, verifier: String, redirectURI: String) async throws {
        try await command("auth/spotify/exchange", body: ["code": code, "code_verifier": verifier, "redirect_uri": redirectURI])
    }
    func spotifyAlbums(offset: Int, limit: Int, query: String) async throws -> SpotifyPage<SpotifyAlbum> {
        try await get(libraryURL("spotify/albums", offset: offset, limit: limit, query: query))
    }
    func spotifyLiked(offset: Int, limit: Int, query: String) async throws -> SpotifyPage<SpotifyTrack> {
        try await get(libraryURL("spotify/tracks", offset: offset, limit: limit, query: query))
    }
    func spotifyAlbumTracks(id: String) async throws -> [SpotifyTrack] {
        try await get(url("spotify/albums/\(id.addingPercentEncoding(withAllowedCharacters: .alphanumerics) ?? id)/tracks"))
    }
    /// Albums and tracks matching `query` anywhere in Spotify's catalog.
    func spotifySearch(query: String) async throws -> SpotifySearchResults {
        var components = URLComponents(url: url("spotify/search"), resolvingAgainstBaseURL: false)!
        components.queryItems = [URLQueryItem(name: "q", value: query)]
        components.percentEncodedQuery = components.percentEncodedQuery?.replacingOccurrences(of: "+", with: "%2B")
        return try await get(components.url!)
    }
    private func libraryURL(_ path: String, offset: Int, limit: Int, query: String) -> URL {
        var components = URLComponents(url: url(path), resolvingAgainstBaseURL: false)!
        components.queryItems = [URLQueryItem(name: "offset", value: String(offset)), URLQueryItem(name: "limit", value: String(limit))]
            + (query.isEmpty ? [] : [URLQueryItem(name: "q", value: query)])
        components.percentEncodedQuery = components.percentEncodedQuery?.replacingOccurrences(of: "+", with: "%2B")
        return components.url!
    }

    func command(_ path: String, body: [String: Any]) async throws {
        var request = URLRequest(url: url(path))
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.httpBody = try JSONSerialization.data(withJSONObject: body)
        _ = try await data(request)
    }

    func stateURL(room: String) -> URL {
        let allowed = CharacterSet(charactersIn: "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-._~")
        return url("zones/\(room.addingPercentEncoding(withAllowedCharacters: allowed)!)/state")
    }

    func events(lastEventID: String?, opened: () -> Void, receive: (DaemonEvent) async -> Void) async throws {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.timeoutIntervalForRequest = 40
        configuration.timeoutIntervalForResource = 7 * 24 * 60 * 60
        let stream = URLSession(configuration: configuration)
        var lastActivity = ContinuousClock.now
        let watchdog = Task {
            while !Task.isCancelled {
                try await Task.sleep(for: .seconds(1))
                if lastActivity.duration(to: .now) > .seconds(40) {
                    stream.invalidateAndCancel()
                    return
                }
            }
        }
        defer { watchdog.cancel(); stream.invalidateAndCancel() }
        var request = URLRequest(url: url("events"))
        request.setValue("text/event-stream", forHTTPHeaderField: "Accept")
        if let lastEventID, !lastEventID.isEmpty {
            request.setValue(lastEventID, forHTTPHeaderField: "Last-Event-ID")
        }
        try await withTaskCancellationHandler {
            let (bytes, response) = try await stream.bytes(for: request)
            try Task.checkCancellation()
            guard let http = response as? HTTPURLResponse else { throw URLError(.badServerResponse) }
            guard http.statusCode == 200 else {
                // The daemon answered and said no. Read its reason so the app can show it.
                var body = Data()
                for try await byte in bytes {
                    body.append(byte)
                    if body.count >= 4096 { break }
                }
                let failure = Self.failure(status: http.statusCode, body: body)
                AppLog.shared.add("stream", "events refused (HTTP \(failure.status)) \(failure.code ?? "-"): \(failure.detail)")
                throw failure
            }
            guard http.value(forHTTPHeaderField: "Content-Type")?.hasPrefix("text/event-stream") == true else {
                AppLog.shared.add("stream", "events answered with \(http.value(forHTTPHeaderField: "Content-Type") ?? "no content type")")
                throw URLError(.cannotParseResponse)
            }
            lastActivity = .now
            AppLog.shared.add("stream", "events open, resuming after \(lastEventID ?? "start")")
            opened()
            var parser = SSEParser()
            // `bytes.lines` drops empty lines, and an empty line is what ends an SSE frame, so split on newlines by hand.
            var line: [UInt8] = []
            for try await byte in bytes {
                try Task.checkCancellation()
                lastActivity = .now
                guard byte == 0x0A else { line.append(byte); continue }
                let text = String(decoding: line, as: UTF8.self)
                line.removeAll(keepingCapacity: true)
                if let event = parser.consume(text) { await receive(event) }
            }
            throw URLError(.networkConnectionLost)
        } onCancel: {
            stream.invalidateAndCancel()
        }
    }

    private func url(_ path: String) -> URL {
        URL(string: baseURL.absoluteString.trimmingCharacters(in: CharacterSet(charactersIn: "/")) + "/" + path)!
    }
    private func get<T: Decodable>(_ url: URL) async throws -> T {
        try JSONDecoder().decode(T.self, from: await data(URLRequest(url: url)))
    }
    private func data(_ request: URLRequest) async throws -> Data {
        let label = "\(request.httpMethod ?? "GET") \(request.url?.path ?? "?")"
        let body: Data
        let response: URLResponse
        do {
            (body, response) = try await session.data(for: request)
        } catch {
            if !(error is CancellationError) { AppLog.shared.add("http", "\(label) failed: \(error.localizedDescription)") }
            throw error
        }
        try Task.checkCancellation()
        guard let http = response as? HTTPURLResponse else { throw URLError(.badServerResponse) }
        guard (200..<300).contains(http.statusCode) else {
            let failure = Self.failure(status: http.statusCode, body: body)
            AppLog.shared.add("http", "\(label) -> HTTP \(failure.status) \(failure.code ?? "-"): \(failure.detail)")
            throw failure
        }
        if request.httpMethod == "POST" { AppLog.shared.add("http", "\(label) ok") }
        return body
    }

    /// The daemon's error body when it sent one, else a plain note with the status.
    static func failure(status: Int, body: Data) -> DaemonFailure {
        var failure = (try? JSONDecoder().decode(DaemonFailure.self, from: body)) ??
            DaemonFailure(detail: "Daemon returned HTTP \(status)", code: nil, hint: nil, suggestions: nil, retryable: nil)
        failure.status = status
        return failure
    }
}
