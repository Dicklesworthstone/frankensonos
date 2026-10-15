import CryptoKit
import Foundation

/// The app's half of Spotify's authorization-code flow with PKCE (RFC 7636). The app makes the verifier, sends the
/// challenge to Spotify, and hands the returned code plus the verifier to the daemon, which owns the tokens.
enum SpotifyAuth {
    /// What the daemon asks Spotify for (crates/fsonos-spotify `SCOPE`).
    static let scope = "user-library-read"

    static func makeVerifier() -> String {
        base64URL(Data((0..<64).map { _ in UInt8.random(in: .min ... .max) }))
    }

    static func makeState() -> String {
        base64URL(Data((0..<16).map { _ in UInt8.random(in: .min ... .max) }))
    }

    static func challenge(for verifier: String) -> String {
        base64URL(Data(SHA256.hash(data: Data(verifier.utf8))))
    }

    static func authorizeURL(clientID: String, redirectURI: String, challenge: String, state: String) -> URL {
        var parts = URLComponents(string: "https://accounts.spotify.com/authorize")!
        parts.queryItems = [
            URLQueryItem(name: "response_type", value: "code"),
            URLQueryItem(name: "client_id", value: clientID),
            URLQueryItem(name: "redirect_uri", value: redirectURI),
            URLQueryItem(name: "code_challenge_method", value: "S256"),
            URLQueryItem(name: "code_challenge", value: challenge),
            URLQueryItem(name: "state", value: state),
            URLQueryItem(name: "scope", value: scope)
        ]
        return parts.url!
    }

    /// The code in Spotify's redirect. The state must be the one this sign-in sent, whatever else the URL says.
    static func code(from callback: URL, expectedState: String) throws -> String {
        let items = URLComponents(url: callback, resolvingAgainstBaseURL: false)?.queryItems ?? []
        func value(_ name: String) -> String? { items.first { $0.name == name }?.value }
        guard value("state") == expectedState else { throw SpotifyAuthError.stateMismatch }
        if let error = value("error") { throw SpotifyAuthError.denied(error) }
        guard let code = value("code"), !code.isEmpty else { throw SpotifyAuthError.missingCode }
        return code
    }

    private static func base64URL(_ data: Data) -> String {
        data.base64EncodedString().replacingOccurrences(of: "+", with: "-").replacingOccurrences(of: "/", with: "_").replacingOccurrences(of: "=", with: "")
    }
}

enum SpotifyAuthError: Error, Equatable, LocalizedError {
    case denied(String)
    case stateMismatch
    case missingCode
    case notConfigured

    var errorDescription: String? {
        switch self {
        case .denied(let reason): "Spotify refused the sign-in: \(reason)."
        case .stateMismatch: "Spotify's reply did not match this sign-in, so it was ignored. Try again."
        case .missingCode: "Spotify's reply carried no code. Try again."
        case .notConfigured: "The daemon has no Spotify Client ID yet."
        }
    }
}

/// Thrown by a browser closure when the person closes the sign-in sheet. Not an error to show.
struct SpotifySignInCancelled: Error {}
