//! FrankenSonos HTTP control API.
//!
//! A small JSON HTTP API over the daemon (`fsonos-core`) built with
//! `fastapi_rust`: list zones, get/set transport and volume, group/ungroup,
//! and drive the DJ. It binds on the loopback and the Tailscale interface so
//! that off-LAN agents reach it over the tailnet (the speakers never leave the
//! LAN; the daemon is the only thing Tailscale fronts). [`app`] assembles the
//! routes.
//!
//! The transport-agnostic layer is shared with the MCP server, so an agent
//! gets the same answer from either surface:
//!
//! * [`request`] — the JSON request bodies and their validation;
//! * [`source`] — canonicalizing the `source_uri` a caller pastes;
//! * [`plan`] — resolving a request to coordinator-addressed [`Command`]s;
//! * [`zones`] — the zone (group) listings;
//! * [`failure`] — the one [`Failure`] shape (status + agent-readable detail).

pub mod failure;
pub mod plan;
pub mod request;
pub mod source;
pub mod zones;

pub use failure::Failure;
pub use plan::Command;
pub use request::{GroupRequest, PlayRequest, VolumeChange, VolumeRequest, ZoneRequest};
pub use zones::ZoneDto;

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

/// A uniform API error payload (FastAPI-style `{ "detail": ... }`).
#[derive(Debug, Serialize, Deserialize)]
pub struct ApiError {
    pub detail: String,
}
