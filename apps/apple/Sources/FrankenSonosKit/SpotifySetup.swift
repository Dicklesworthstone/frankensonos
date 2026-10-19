import Foundation

/// SignInDto reports a cached grant, not a successful library import or proof
/// that Spotify/Sonos playback is ready. The daemon alone stores credentials.
public struct SpotifySignIn: Decodable, Equatable, Sendable {
    public let signedIn: Bool
    public let pending: Bool
    public let redirectUri: String

    public func validated() throws -> Self {
        _ = try SpotifyAddress.redirect(redirectUri)
        return self
    }
}

/// A short-lived consent link. Do not persist it or log its OAuth state.
public struct SpotifyConsent: Decodable, Equatable, Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    private let url: String
    public let redirectUri: String
    public var description: String { "Spotify consent (authorization URL redacted)" }
    public var debugDescription: String { description }

    /// Only the existing read-only PKCE flow may be opened by the native UI.
    /// A daemon response is not permission to navigate to an arbitrary origin.
    public func authorizationURL() throws -> URL {
        let c = try SpotifyAddress.components(url)
        let fields = try SpotifyAddress.fields(c)
        let allowedScopes: Set<String> = ["user-library-read", "user-follow-read", "playlist-read-private", "user-top-read", "user-read-recently-played"]
        let scopes = Set((fields["scope"] ?? "").split(separator: " ").map(String.init))
        guard c.scheme == "https", c.host?.lowercased() == "accounts.spotify.com",
              c.port == nil || c.port == 443, c.percentEncodedPath == "/authorize",
              fields["response_type"] == "code", fields["code_challenge_method"] == "S256",
              fields["redirect_uri"] == redirectUri,
              fields["client_id"].map({ !$0.isEmpty && $0.utf8.count <= 256 && $0.utf8.allSatisfy(SpotifyAddress.alphanumeric) }) == true,
              fields["code_challenge"].map({ $0.utf8.count == 43 && $0.utf8.allSatisfy(SpotifyAddress.base64url) }) == true,
              fields["state"].map({ !$0.isEmpty && $0.utf8.count <= 512 && $0.utf8.allSatisfy(SpotifyAddress.base64url) }) == true,
              scopes.contains("user-library-read"), scopes.isSubset(of: allowedScopes),
              Set(fields.keys).isSubset(of: ["client_id", "response_type", "redirect_uri", "code_challenge_method", "code_challenge", "scope", "state", "show_dialog"]),
              fields["show_dialog"].map({ ["true", "false"].contains($0) }) ?? true,
              let result = c.url else { throw SpotifySetupError.unsafeConsent }
        _ = try SpotifyAddress.redirect(redirectUri)
        return result
    }

    /// Paste a complete callback address; never visit it from the remote.
    /// Check the exact configured redirect and this flow's state before the
    /// daemon independently checks state and exchanges the one-time code.
    func callback(_ input: String) throws -> String {
        _ = try authorizationURL()
        let text = input.trimmingCharacters(in: .whitespacesAndNewlines)
        do {
            let expected = try SpotifyAddress.redirect(redirectUri)
            let c = try SpotifyAddress.components(text)
            let fields = try SpotifyAddress.fields(c)
            let consent = try SpotifyAddress.fields(SpotifyAddress.components(url))
            guard c.scheme == expected.scheme, c.host?.lowercased() == expected.host?.lowercased(),
                  SpotifyAddress.port(c) == SpotifyAddress.port(expected),
                  c.percentEncodedPath == expected.percentEncodedPath,
                  fields["state"] == consent["state"],
                  Set(fields.keys).isSubset(of: ["code", "state", "error", "error_description"]),
                  let code = fields["code"], !code.isEmpty, code.utf8.count <= 8_192,
                  !code.unicodeScalars.contains(where: CharacterSet.controlCharacters.contains),
                  fields["error"] == nil, fields["error_description"] == nil else { throw SpotifySetupError.invalidCallback }
            return text
        } catch { throw SpotifySetupError.invalidCallback }
    }
}

/// LibrarySyncDto is a job observation. A running job may still carry an older
/// successful import or an earlier failure; neither is the current job's result.
public struct SpotifyLibrarySync: Decodable, Equatable, Sendable {
    public let done: String
    public let running: Bool
    public let last: SpotifyLibraryImport?
    public let error: String?

    public func validated() throws -> Self {
        guard !done.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { throw ClientError.unreadable }
        if let last { _ = try last.validated() }
        return self
    }
}

public struct SpotifyLibraryImport: Decodable, Equatable, Sendable {
    public let at: Int64
    public let tracks: UInt64
    public let candidates: UInt64
    public let classical: UInt64
    public let retired: UInt64
    public let genreArtistsPending: UInt64?
    public let notes: [String]?
    public var incomplete: Bool { (genreArtistsPending ?? 0) > 0 || !(notes ?? []).isEmpty }

    func validated() throws -> Self {
        guard (-62_135_596_800...253_402_300_799).contains(at),
              classical <= candidates, candidates <= tracks else { throw ClientError.unreadable }
        return self
    }
}

public enum SpotifySetupError: Error, LocalizedError, Equatable, Sendable {
    case unsafeConsent, invalidCallback, uncertain, changedSignIn
    public var errorDescription: String? {
        switch self {
        case .unsafeConsent: "The daemon did not return the expected read-only Spotify consent link. Nothing was opened."
        case .invalidCallback: "Paste the complete callback address from this sign-in, with its matching state and authorization code. It was not sent. A denied or replaced sign-in needs a new consent flow."
        case .uncertain: "The Spotify request may have taken effect. It was not repeated. Refresh status before starting another request; callback details are not shown."
        case .changedSignIn: "The daemon's sign-in changed. Refresh its status and review whether to replace the pending flow. Nothing was sent."
        }
    }
}

extension DaemonClient {
    public func spotifySignIn() async throws -> SpotifySignIn {
        try await get(SpotifySignIn.self, ["auth", "spotify"]).validated()
    }
    public func spotifyLibrarySync() async throws -> SpotifyLibrarySync {
        try await get(SpotifyLibrarySync.self, ["dj", "sync"]).validated()
    }
}

private enum SpotifyAddress {
    static func alphanumeric(_ b: UInt8) -> Bool { (48...57).contains(b) || (65...90).contains(b) || (97...122).contains(b) }
    static func base64url(_ b: UInt8) -> Bool { alphanumeric(b) || b == 45 || b == 95 }
    static func port(_ c: URLComponents) -> Int { c.port ?? (c.scheme == "https" ? 443 : 80) }

    static func components(_ input: String) throws -> URLComponents {
        let bytes = Array(input.utf8)
        guard !bytes.isEmpty, bytes.count <= 16_384,
              !input.unicodeScalars.contains(where: { CharacterSet.whitespacesAndNewlines.contains($0) || CharacterSet.controlCharacters.contains($0) }),
              !input.contains("\\") else { throw SpotifySetupError.unsafeConsent }
        // URLComponents repairs malformed percent escapes; do not accept a
        // repaired authorization/callback identity as the reviewed address.
        func hex(_ b: UInt8) -> Bool { (48...57).contains(b) || (65...70).contains(b) || (97...102).contains(b) }
        for i in bytes.indices where bytes[i] == 37 {
            guard i + 2 < bytes.count, hex(bytes[i + 1]), hex(bytes[i + 2]) else { throw SpotifySetupError.unsafeConsent }
        }
        guard let c = URLComponents(string: input), let host = c.host, !host.isEmpty,
              c.user == nil, c.password == nil, c.fragment == nil,
              c.port.map({ (1...65535).contains($0) }) ?? true else { throw SpotifySetupError.unsafeConsent }
        return c
    }
    static func redirect(_ text: String) throws -> URLComponents {
        let c = try components(text)
        guard c.query == nil, c.scheme == "https" || (c.scheme == "http" && ["127.0.0.1", "[::1]", "::1"].contains(c.host ?? "")) else {
            throw SpotifySetupError.unsafeConsent
        }
        return c
    }
    static func fields(_ c: URLComponents) throws -> [String: String] {
        var result: [String: String] = [:]
        // Decode form spaces before percent escapes, so a literal %2B in a
        // code or redirect path remains + rather than becoming a space.
        var form = c
        form.percentEncodedQuery = c.percentEncodedQuery?.replacingOccurrences(of: "+", with: "%20")
        for item in form.queryItems ?? [] {
            guard let value = item.value, result[item.name] == nil else { throw SpotifySetupError.unsafeConsent }
            result[item.name] = value
        }
        return result
    }
}
