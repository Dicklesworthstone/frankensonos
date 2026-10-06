//! Spotify Web API client (read-only, user's own library).
//!
//! Scopes needed: `user-library-read` (saved albums + liked tracks). Auth is
//! Authorization Code with PKCE; tokens are cached locally (never committed).
//! The HTTPS calls are wired over the franken client in FND-DEPS; this file
//! defines the shapes and the PKCE helper so the auth flow is testable.

/// Generate a PKCE `code_verifier` / `code_challenge` pair (S256). The
/// challenge is the base64url-unpadded SHA-256 of the verifier. Implemented in
/// FND-DEPS once a sha2 dep is added; the signature is fixed here so the auth
/// module can be written against it.
#[derive(Debug, Clone)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

/// Configuration for the Spotify client. The client id is not a secret for
/// PKCE flows, but the refresh token IS and is stored only in the local,
/// git-ignored auth cache.
#[derive(Debug, Clone)]
pub struct SpotifyConfig {
    pub client_id: String,
    pub redirect_uri: String,
}
