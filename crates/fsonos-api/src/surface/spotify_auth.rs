//! The Spotify sign-in through the daemon. The daemon holds the PKCE
//! verifier and `state`, so the owner's browser comes back to the daemon's
//! own port (the registered redirect URI, by default
//! http://127.0.0.1:8099/auth/spotify/callback) and `fsonos setup` never
//! needs a second listener there. fsonos-spotify runs the flow behind
//! [`SpotifyAuth`] (implemented in fsonos-cli); this is the surfaces' side:
//! `GET /auth/spotify`, `POST /auth/spotify/begin`, the callback, and
//! `POST /auth/spotify/complete` for an address pasted from a browser
//! elsewhere.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::policy::Client;
use serde::{Deserialize, Serialize};

use super::Surface;
use crate::failure::{ErrorCode, Failure};

/// The tool every sign-in call is authorized as.
pub const SIGN_IN_TOOL: &str = "spotify_sign_in";

/// Runs the owner's Spotify sign-in; see the module docs.
pub trait SpotifyAuth: Send + Sync {
    /// Whether the owner is signed in, and whether a sign-in waits for the
    /// browser.
    fn status(&self) -> SignInDto;

    /// Start a sign-in: the consent page to open. It replaces any sign-in
    /// still waiting.
    fn begin(&self) -> Result<SignInStartDto, Failure>;

    /// Finish the waiting sign-in with the address the browser landed on (a
    /// request target, a full URL, or its query): `state` is checked, then
    /// the code is exchanged and the token cached.
    fn complete(&self, redirect: &str) -> Result<(), Failure>;
}

/// `GET /auth/spotify`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SignInDto {
    /// A token is cached: the daemon can read the owner's library.
    pub signed_in: bool,
    /// A sign-in waits for the browser to come back.
    pub pending: bool,
    /// Where Spotify sends the browser back to.
    pub redirect_uri: String,
}

/// `POST /auth/spotify/begin`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SignInStartDto {
    /// The consent page to open in a browser.
    pub url: String,
    pub redirect_uri: String,
}

/// `POST /auth/spotify/complete` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SignInCompleteRequest {
    /// The address the browser landed on after allowing access.
    pub redirect: String,
}

impl Surface {
    /// Run the Spotify sign-in with `engine`.
    #[must_use]
    pub fn with_spotify_auth(mut self, engine: Box<dyn SpotifyAuth>) -> Self {
        self.spotify_auth = Some(engine);
        self
    }

    fn sign_in_engine(&self) -> Result<&dyn SpotifyAuth, Failure> {
        self.spotify_auth.as_deref().ok_or_else(|| {
            Failure::new(
                ErrorCode::NotImplemented,
                "this daemon has no Spotify app configured",
            )
            .with_hint("Start fsonos serve with FSONOS_SPOTIFY_CLIENT_ID (or --spotify-client-id).")
        })
    }

    /// Whether the owner is signed in to Spotify (`GET /auth/spotify`).
    pub fn spotify_sign_in_status(&self, client: &Client) -> Result<SignInDto, Failure> {
        self.guard(client).authorize(SIGN_IN_TOOL, true)?;
        Ok(self.sign_in_engine()?.status())
    }

    /// Start a sign-in (`POST /auth/spotify/begin`).
    pub fn spotify_sign_in_begin(&self, client: &Client) -> Result<SignInStartDto, Failure> {
        self.guard(client).authorize(SIGN_IN_TOOL, false)?;
        self.sign_in_engine()?.begin()
    }

    /// Finish the waiting sign-in with the browser's redirect (the callback,
    /// or `POST /auth/spotify/complete`).
    pub fn spotify_sign_in_complete(
        &self,
        client: &Client,
        redirect: &str,
    ) -> Result<SignInDto, Failure> {
        self.guard(client).authorize(SIGN_IN_TOOL, false)?;
        let engine = self.sign_in_engine()?;
        engine.complete(redirect)?;
        Ok(engine.status())
    }
}
