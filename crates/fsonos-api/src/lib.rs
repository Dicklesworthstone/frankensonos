//! FrankenSonos HTTP control API.
//!
//! A small JSON HTTP API over the daemon (`fsonos-core`) built with
//! `fastapi_rust`: list zones, get/set transport and volume, group/ungroup,
//! and drive the DJ. It binds on the loopback and the Tailscale interface so
//! that off-LAN agents reach it over the tailnet (the speakers never leave the
//! LAN; the daemon is the only thing Tailscale fronts). Route wiring to fastapi
//! lands in FND-DEPS; the request/response DTOs and the pure handler logic live
//! here so they are testable now.

use serde::{Deserialize, Serialize};

/// `GET /zones` response item.
#[derive(Debug, Serialize, Deserialize)]
pub struct ZoneDto {
    pub coordinator_room: String,
    pub members: Vec<String>,
    pub transport_state: String,
}

/// `POST /play` request body.
#[derive(Debug, Serialize, Deserialize)]
pub struct PlayRequest {
    pub zone: String,
    pub source_uri: String,
    #[serde(default)]
    pub title: Option<String>,
}

/// A uniform API error payload (FastAPI-style `{ "detail": ... }`).
#[derive(Debug, Serialize, Deserialize)]
pub struct ApiError {
    pub detail: String,
}
