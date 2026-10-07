//! Spotify Web API client + the classical-music DJ engine.
//!
//! Six concerns:
//!
//! * [`client`] — a Spotify Web API client (OAuth Authorization Code + PKCE)
//!   used **only to read the user's own library**: saved albums and liked
//!   tracks. It does NOT start playback — Sonos renders Spotify itself via
//!   SMAPI (`fsonos_proto::didl`); the Web API cannot command a Sonos.
//!
//! * [`session`] — the I/O half: the owner's authorization, the local token
//!   cache, and authorized reads over the asupersync HTTPS client.
//!
//! * [`library`] — [`library::LibraryItem`], the neutral track metadata both
//!   the Web API reads and the local library cache produce.
//!
//! * [`classical`] — metadata heuristics (is it classical? composer, period,
//!   work, energy) and the DJ's [`classical::CandidatePool`].
//!
//! * [`works`] — whole works: a piece's movements grouped and in disc/track
//!   order, with a completeness flag (the DJ's unit of selection).
//!
//! * [`dj`] — the DJ: given the pool plus recent play history, pick the next
//!   track for pleasant variety (spread across composers/periods/works, fit
//!   the time of day, avoid recent repeats). Pure logic, fully testable
//!   without any network.

pub mod classical;
pub mod client;
pub mod dj;
pub mod library;
pub mod session;
pub mod works;

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
    #[error("http error: {0}")]
    Http(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
