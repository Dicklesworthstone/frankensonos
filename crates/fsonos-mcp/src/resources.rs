//! The house as MCP resources: documents an agent can read, and read again,
//! instead of calling a tool.
//!
//! * `sonos://zones`: every zone (the `list_zones` answer, as JSON);
//! * `sonos://zones/{room}`: one room's zone state (the `get_zone_state`
//!   answer). `room` is percent-encoded (`Living%20Room`).
//!
//! Change notifications are not sent yet: fastmcp_rust (at the pinned
//! revision) publishes `notifications/resources/updated` only from inside a
//! live request, and the daemon's changes arrive from its event bus with no
//! request in flight. HTTP clients can follow the same changes on
//! `GET /events`.

use fastmcp::prelude::*;
use fsonos_api::Failure;

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

/// Every zone (group of rooms playing together) in both households: its
/// rooms, its household, and whether it is playing.
#[resource(uri = "sonos://zones", mime_type = "application/json")]
fn zones() -> McpResult<String> {
    backend()
        .and_then(crate::tools::Backend::zones_document)
        .map_err(|f| read_error(&f))
}

/// One room's zone state: its group, what the group plays, the room's
/// volume. `room` is a percent-encoded room name.
#[resource(uri = "sonos://zones/{room}", mime_type = "application/json")]
fn zone(room: String) -> McpResult<String> {
    let room = fsonos_api::http::percent_decode(&room).ok_or_else(move || {
        McpError::invalid_params(format!("room {room:?} is not valid percent-encoded UTF-8"))
    })?;
    backend()
        .and_then(|b| b.zone_document(&room))
        .map_err(|f| read_error(&f))
}
