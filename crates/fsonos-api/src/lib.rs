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
//! * [`dj`] — the DJ engine a surface runs `dj_*` commands with, and its
//!   status, steering and moods as the surfaces show them;
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
pub mod off_thread;
pub mod plan;
pub mod reads;
pub mod request;
pub mod source;
pub mod surface;
pub mod web;
pub mod zones;

pub use dj::{
    DjFactorDto, DjMoodDto, DjMoodsDto, DjProgramDto, DjReasonDto, DjStatusDto, DjSteer,
    DjSteeringDto, DjWorkDto, SteerConstraints,
};
pub use execute::{OutcomeDto, execute, execute_guarded};
pub use failure::{ErrorCode, Failure, NoteCode};
pub use guard::{Guard, Note};
pub use http::app;
pub use identity::Identity;
pub use log::{ActionDto, ActionsQuery, UndoDto, UndoRequest};
pub use plan::Command;
pub use reads::{FavoriteDto, HitDto, PlayDto, RoomDto, TrackDto, ZoneStateDto};
pub use request::{
    DjStartRequest, DjSteerRequest, GroupRequest, MoveRequest, MuteRequest, PartyRequest,
    PlayFavoriteRequest, PlayRequest, SearchPlayRequest, SearchRequest, SeekRequest, VolumeChange,
    VolumeRequest, ZoneRequest,
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
    /// The last full `fsonos selftest` run kept in the daemon's data
    /// directory (the daily one, when scheduled), without its details.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selftest: Option<SelftestHealth>,
    /// What the daemon's live model sees, so a daemon that answers but
    /// cannot reach the speakers says so. Counts only: no address or name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live: Option<LiveHealth>,
    /// The room left on the volume that holds the daemon's store: when it
    /// runs out, actions go unlogged and cannot be undone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk: Option<DiskHealth>,
    /// Storage faults the daemon's store met (a full disk, corruption, I/O
    /// errors), when it met any: while they last, actions are not logged or
    /// undoable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store: Option<StoreHealth>,
    /// The daemon's own warnings that are storming (co55.7): one kind logged
    /// far more often than usual in the last hour. Absent while none is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warnings: Option<WarningsHealth>,
}

/// The kinds of warning storming in the daemon, in `GET /health` (`fsonos
/// doctor` judges them as `daemon.warnings`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WarningsHealth {
    pub storming: Vec<StormHealth>,
}

/// One kind of warning storming in the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StormHealth {
    /// Where it is logged (`target@file:line`), never its field values.
    pub kind: String,
    /// `warn` or `error`.
    pub level: String,
    /// Its message, every word holding a digit masked as `N`.
    pub sample: String,
    /// How many in the last hour.
    pub last_hour: u32,
    /// How many make a storm for it: at least 20, or ten times its usual
    /// hour.
    pub threshold: u32,
}

/// The store's failures, in `GET /health` (vg3s.8; `fsonos doctor` fails
/// `daemon.health` while they are recent).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StoreHealth {
    /// The newest one's kind: `disk_full`, `corrupt` or `io`.
    pub kind: String,
    /// How many since the daemon started.
    pub failures: u64,
    pub last_secs_ago: u64,
}

/// The store's volume, in `GET /health` (`fsonos doctor` judges it as
/// `store.disk`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DiskHealth {
    /// `ok`; `low` (under 5 GiB: doctor warns); or `critical` (under 1 GiB:
    /// doctor fails, and store writes are about to).
    pub verdict: String,
    pub free_bytes: u64,
    pub total_bytes: u64,
}

/// The live model, in `GET /health` ([`live::health`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LiveHealth {
    /// `ok`; `degraded` (players offline, or no event subscriptions);
    /// `starting` (its first survey has not ended); or `blind` (no player
    /// found: the speakers are out of reach).
    pub verdict: String,
    pub households: u32,
    pub players: u32,
    pub offline: u32,
    pub subscriptions: u32,
    /// Seconds since a survey last succeeded; absent before the first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surveyed_secs_ago: Option<u64>,
}

/// The last self-test, in `GET /health`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SelftestHealth {
    /// When it ended, in Unix seconds.
    pub at: u64,
    /// Every check passed (`fsonos doctor` names any that did not).
    pub passed: bool,
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
