//! FrankenSonos HTTP control API.
//!
//! A small JSON HTTP API over the daemon (`fsonos-core`) built with
//! `fastapi_rust`: list zones, get/set transport and volume, group/ungroup,
//! and drive the DJ. It binds on the loopback and the Tailscale interface so
//! that off-LAN agents reach it over the tailnet (the speakers never leave the
//! LAN; the daemon is the only thing Tailscale fronts). [`app`] assembles the
//! routes; the request/response DTOs and the pure handler logic live here so
//! they are testable without a socket.

use fastapi::{App, Request, RequestContext, Response};
use serde::{Deserialize, Serialize};
use std::future::{Ready, ready};

/// `GET /health` response.
#[derive(Debug, Serialize, Deserialize)]
pub struct HealthDto {
    pub status: String,
    pub version: String,
}

fn health(_cx: &RequestContext, _req: &mut Request) -> Ready<Response> {
    let body = HealthDto {
        status: "ok".into(),
        version: env!("CARGO_PKG_VERSION").into(),
    };
    ready(Response::json(&body).expect("HealthDto serializes"))
}

/// Build the HTTP API application. Routes are registered with the builder,
/// not the `#[get]` macros (see the workspace `Cargo.toml` note on unsafe).
#[must_use]
pub fn app() -> App {
    App::builder().get("/health", health).build()
}

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
