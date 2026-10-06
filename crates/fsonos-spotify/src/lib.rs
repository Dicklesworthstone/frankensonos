//! Spotify Web API client + the classical-music DJ engine.
//!
//! Two concerns:
//!
//! * [`client`] — a Spotify Web API client (OAuth Authorization Code + PKCE)
//!   used **only to read the user's own library**: saved albums and liked
//!   tracks, and track/artist metadata. It does NOT start playback — Sonos
//!   renders Spotify itself via SMAPI (see [`fsonos_proto::didl`]); the Web API
//!   cannot command a Sonos. The HTTPS transport is wired in FND-DEPS.
//!
//! * [`dj`] — the DJ: given a pool of the user's classical tracks plus recent
//!   play history, pick the next track for pleasant variety (spread across
//!   composers/periods/works, avoid recent repeats). This is pure logic and
//!   fully testable without any network.

pub mod client;
pub mod dj;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum SpotifyError {
    #[error("auth error: {0}")]
    Auth(String),
    #[error("api error {status}: {body}")]
    Api { status: u16, body: String },
    #[error("transport not yet wired (FND-DEPS)")]
    NotWired,
}
