//! Spotify Web API client + the classical-music DJ engine.
//!
//! Four concerns:
//!
//! * [`client`] — a Spotify Web API client (OAuth Authorization Code + PKCE)
//!   used **only to read the user's own library**: saved albums and liked
//!   tracks. It does NOT start playback — Sonos renders Spotify itself via
//!   SMAPI (`fsonos_proto::didl`); the Web API cannot command a Sonos. The
//!   HTTPS transport is wired in FND-DEPS.
//!
//! * [`library`] — [`library::LibraryItem`], the neutral track metadata both
//!   the Web API reads and the local library cache produce.
//!
//! * [`classical`] — metadata heuristics (is it classical? composer, period,
//!   work, energy) and the DJ's [`classical::CandidatePool`].
//!
//! * [`dj`] — the DJ: given the pool plus recent play history, pick the next
//!   track for pleasant variety (spread across composers/periods/works, fit
//!   the time of day, avoid recent repeats). Pure logic, fully testable
//!   without any network.

pub mod classical;
pub mod client;
pub mod dj;
pub mod library;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum SpotifyError {
    #[error("auth error: {0}")]
    Auth(String),
    #[error("api error {status}: {body}")]
    Api { status: u16, body: String },
    #[error("invalid config: {0}")]
    Config(String),
    #[error("decode error: {0}")]
    Decode(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("transport not yet wired (FND-DEPS)")]
    NotWired,
}
