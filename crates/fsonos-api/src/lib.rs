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
//! * [`execute`] — carrying a [`Command`] out on the speakers;
//! * [`zones`] — the zone (group) listings;
//! * [`failure`] — the one [`Failure`] shape (status + agent-readable detail).

pub mod execute;
pub mod failure;
pub mod plan;
pub mod request;
pub mod source;
pub mod zones;

pub use execute::{OutcomeDto, execute};
pub use failure::{ErrorCode, Failure, NoteCode};
pub use plan::Command;
pub use request::{
    GroupRequest, MuteRequest, PlayRequest, VolumeChange, VolumeRequest, ZoneRequest,
};
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

/// The API's error body (FastAPI-style `detail`, plus the stable code). See
/// [`Failure`] and `docs/ERRORS.md`. Errors the HTTP framework itself raises
/// (an unknown route, say) carry only `detail`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    pub detail: String,
    pub code: ErrorCode,
    pub hint: String,
    pub suggestions: Vec<String>,
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upnp_code: Option<u16>,
}
