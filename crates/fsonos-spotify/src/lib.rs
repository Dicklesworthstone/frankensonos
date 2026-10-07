//! Spotify Web API client + the classical-music DJ engine.
//!
//! Eight concerns:
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
//! * [`cache`] — the library cache: syncing a library read into the store
//!   and rebuilding the DJ's pool from it.
//!
//! * [`classical`] — metadata heuristics (is it classical? composer, period,
//!   work, energy) and the DJ's [`classical::CandidatePool`].
//!
//! * [`works`] — whole works: a piece's movements grouped and in disc/track
//!   order, with a completeness flag (the DJ's unit of selection).
//!
//! * [`steer`] — steering: structured constraints, named moods, and the
//!   session that carries them; applied as hard filters before weighting.
//!
//! * [`dj`] — the DJ: given the pool plus recent play history, pick the next
//!   track for pleasant variety (spread across composers/periods/works, fit
//!   the time of day, avoid recent repeats). Pure logic, fully testable
//!   without any network.

pub mod cache;
pub mod classical;
pub mod client;
pub mod dj;
#[cfg(test)]
mod fake_spotify;
pub mod library;
pub mod session;
pub mod steer;
#[cfg(test)]
mod test_shelf;
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
    #[error("store error: {0}")]
    Store(#[from] fsonos_core::store::StoreError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
