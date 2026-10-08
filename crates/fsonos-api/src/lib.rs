//! FrankenSonos HTTP control API.
//!
//! A small JSON HTTP API over the daemon (`fsonos-core`) built with
//! `fastapi_rust`: list zones, get/set transport and volume, group/ungroup,
//! and drive the DJ. It binds on the loopback and the Tailscale interface so
//! that off-LAN agents reach it over the tailnet (the speakers never leave the
//! LAN; the daemon is the only thing Tailscale fronts). [`app`] assembles the
//! routes ([`http`] lists them).
//!
//! The transport-agnostic layer is shared with the MCP server, so an agent
//! gets the same answer from either surface:
//!
//! * [`request`] — the JSON request bodies and their validation;
//! * [`source`] — canonicalizing the `source_uri` a caller pastes;
//! * [`plan`] — resolving a request to coordinator-addressed [`Command`]s;
//! * [`execute`] — carrying a [`Command`] out on the speakers;
//! * [`guard`] — the house policy: who may call what, how loud;
//! * [`heal`] — retrying once when the speakers changed under a command;
//! * [`surface`] — the speakers a surface acts on, with its policy;
//! * [`web`] — browser safety: Host, Origin, JSON-only writes;
//! * [`identity`] — who an HTTP request comes from (Serve's login header);
//! * [`zones`] — the zone (group) listings; [`reads`] — zone state, favorites;
//! * [`log`] — the action log and undo, as the surfaces show them;
//! * [`live`] — what the surfaces read from the daemon's live model;
//! * [`events`] — the event stream (`GET /events`);
//! * [`dj`] — the DJ engine a surface runs `dj_*` commands with;
//! * [`failure`] — the one [`Failure`] shape (status + agent-readable detail).

pub mod dj;
pub mod events;
pub mod execute;
pub mod failure;
pub mod guard;
pub mod heal;
pub mod http;
pub mod identity;
pub mod live;
pub mod log;
pub mod plan;
pub mod reads;
pub mod request;
pub mod source;
pub mod surface;
pub mod web;
pub mod zones;

pub use execute::{OutcomeDto, execute, execute_guarded};
pub use failure::{ErrorCode, Failure, NoteCode};
pub use guard::{Guard, Note};
pub use http::app;
pub use identity::Identity;
pub use log::{ActionDto, ActionsQuery, UndoDto, UndoRequest};
pub use plan::Command;
pub use reads::{FavoriteDto, HitDto, PlayDto, RoomDto, TrackDto, ZoneStateDto};
pub use request::{
    GroupRequest, MoveRequest, MuteRequest, PartyRequest, PlayFavoriteRequest, PlayRequest,
    SearchRequest, VolumeChange, VolumeRequest, ZoneRequest,
};
pub use surface::Surface;
pub use web::WebPolicy;
pub use zones::ZoneDto;

use fastapi::{JsonSchema, fastapi_openapi};
use serde::{Deserialize, Serialize};

/// `GET /health` response.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct HealthDto {
    pub status: String,
    pub version: String,
}

/// The API's error body (FastAPI-style `detail`, plus the stable code). See
/// [`Failure`] and `docs/ERRORS.md`. Errors the HTTP framework itself raises
/// (an unknown route, say) carry only `detail`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ApiError {
    pub detail: String,
    pub code: ErrorCode,
    pub hint: String,
    pub suggestions: Vec<String>,
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upnp_code: Option<u16>,
}
