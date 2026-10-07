//! Shared domain types for FrankenSonos.
//!
//! These types are the common vocabulary spoken by every other crate. They are
//! pure data: no I/O, no async, no franken-stack dependencies. Keep them that
//! way so they compile everywhere (including future wasm/embedded targets).

pub mod text;

use serde::{Deserialize, Serialize};

/// Which Sonos software generation a household speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Generation {
    /// Legacy S1 line (e.g. Play:5 Gen 1, Bridge). SonosNet-capable.
    S1,
    /// Current S2 line (e.g. Play:1, Sonos One). WiFi.
    S2,
}

/// Stable identifier for a household (Sonos calls this the household id).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HouseholdId(pub String);

/// Stable identifier for a single player (a UPnP device UUID / RINCON id).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PlayerId(pub String);

/// A single Sonos player (one speaker / zone endpoint).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Player {
    pub id: PlayerId,
    pub room_name: String,
    pub ip: std::net::IpAddr,
    pub model: String,
    pub generation: Generation,
}

/// A zone group: one coordinator plus zero or more member players that render
/// the same audio in sync.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ZoneGroup {
    pub coordinator: PlayerId,
    pub members: Vec<PlayerId>,
}

/// Transport state of a (coordinator) player.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransportState {
    Playing,
    Paused,
    Stopped,
    Transitioning,
    Unknown,
}

/// A music track reference, source-agnostic. The `uri` is the renderer-facing
/// URI (e.g. an `x-sonos-spotify:` URI once resolved); `source_uri` is the
/// human/service-facing id (e.g. `spotify:track:...`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Track {
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub source_uri: String,
    pub uri: Option<String>,
    pub duration_secs: Option<u32>,
}

/// Crate-wide error type.
#[derive(Debug, thiserror::Error)]
pub enum TypeError {
    #[error("invalid identifier: {0}")]
    InvalidId(String),
}
