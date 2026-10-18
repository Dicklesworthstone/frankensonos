//! The house as MCP resources: documents an agent can read, and read again,
//! instead of calling a tool.
//! Reads share the tools' bounded worker budget: waiting on a daemon, a
//! survey, or a speaker never holds the MCP listener's runtime threads.
//!
//! * `sonos://zones`: every zone (the `list_zones` answer, as JSON);
//! * `sonos://zones/{room}`: one room's zone state (the `get_zone_state`
//!   answer). `room` is percent-encoded (`Living%20Room`);
//! * `sonos://dj`: the DJ in every zone (each zone's `dj_status`), with its
//!   moods and time-of-day program (`dj_moods`).
//!
//! Subscribed clients receive resource invalidations from the same authorized
//! event stream as the `events` tool, including while no resource read is in
//! flight. Both protocol eras are supported. Updates preserve the exact URI
//! subscribed, including percent-encoded, household-qualified room names.
//! A missed-event reset or recovery invalidates every subscribed Sonos resource;
//! clients read the documents again to obtain current state. Event delivery
//! needs the daemon's live model, locally or through the selected daemon URL.
//! No event polling or speaker reads are started without a Sonos subscriber.

use fastmcp::prelude::*;
use fsonos_api::{ErrorCode, Failure};

use crate::tools::backend;

/// A failure as a resource read error: the caller's mistakes as invalid
/// parameters, the rest as an error carrying the same text.
fn read_error(failure: &Failure) -> McpError {
    if (400..500).contains(&failure.status()) {
        McpError::invalid_params(failure.tool_text())
    } else {
        McpError::tool_error(failure.tool_text())
    }
}

/// Await a blocking document read within the server's shared worker limit.
async fn document(
    work: impl FnOnce() -> Result<String, Failure> + Send + 'static,
) -> McpResult<String> {
    fsonos_api::off_thread::off_thread(work)
        .map_err(|failure| read_error(&failure))?
        .await
        .ok_or_else(|| {
            read_error(&Failure::new(
                ErrorCode::Internal,
                "the resource read stopped before it answered",
            ))
        })?
        .map_err(|failure| read_error(&failure))
}

/// Every zone (group of rooms playing together) in both households: its
/// rooms, its household, and whether it is playing.
#[resource(uri = "sonos://zones", mime_type = "application/json")]
async fn zones(ctx: &McpContext) -> McpResult<String> {
    ctx.checkpoint()?;
    let backend = backend().map_err(|failure| read_error(&failure))?;
    document(move || backend.zones_document()).await
}

/// One room's zone state: its group, what the group plays, the room's
/// volume. `room` is a percent-encoded room name.
#[resource(uri = "sonos://zones/{room}", mime_type = "application/json")]
async fn zone(ctx: &McpContext, room: String) -> McpResult<String> {
    ctx.checkpoint()?;
    let room = fsonos_api::http::percent_decode(&room).ok_or_else(move || {
        McpError::invalid_params(format!("room {room:?} is not valid percent-encoded UTF-8"))
    })?;
    let backend = backend().map_err(|failure| read_error(&failure))?;
    document(move || backend.zone_document(&room)).await
}

/// The DJ in every zone: whether it feeds the queue, the work it plays and
/// plays next, and how it is steered; with the moods it can be steered to
/// and the time-of-day program that steers it otherwise.
#[resource(uri = "sonos://dj", mime_type = "application/json")]
async fn dj(ctx: &McpContext) -> McpResult<String> {
    ctx.checkpoint()?;
    let backend = backend().map_err(|failure| read_error(&failure))?;
    document(move || backend.dj_document()).await
}
