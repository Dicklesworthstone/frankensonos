//! FrankenSonos MCP server.
//!
//! Exposes the house audio to AI agents (Claude, Grok, Meta Muse, OpenAI "dots"
//! — anything speaking MCP) as well-described tools for reading the house,
//! playback, volume, grouping, the DJ, scenes, schedules and announcements
//! (the README's "For agents" lists them). Served over stdio and streamable HTTP via
//! `fastmcp_rust`. [`server`] builds the server; [`tools`] holds the tools and
//! the [`tools::Backend`] of speakers they act on, installed once per process
//! with [`tools::install`]; [`resources`] serves the zones as documents. The tools take the HTTP API's request bodies
//! (`fsonos_api::request`) and share its validation, planning, execution and
//! house policy, so both surfaces answer alike.

mod play_mode;
mod reviewed_undo;
mod worker;

pub mod resources;
pub mod tools;

use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult, ResultMeta};
use fsonos_api::Failure;

/// Room-name matching is the core's: one normalizer for every surface.
pub use fsonos_core::rooms::normalize_room;

/// Echo `text` back unchanged: a connectivity check an agent can call to prove
/// it reaches the daemon end to end.
#[tool(description = "Echo the given text back unchanged (connectivity check).")]
#[allow(clippy::unused_async)] // the `#[tool]` contract is an async handler
async fn echo(ctx: &McpContext, text: String) -> McpResult<String> {
    ctx.checkpoint()?;
    Ok(text)
}

/// Play a selected existing queue entry through the shared backend.
#[tool(
    description = "Play an existing entry in a room's group queue without adding or removing tracks. First read get_queue and pass its one-based position; optional expected_uri must be the exact renderer URI returned for that entry, so a changed selection is refused. Also pass the queue's opaque revision as expected_revision to reject edits and coordinator changes even with duplicate track URIs. On a mismatch read the queue again; never silently discard these checks. Activates the queue even when a radio or another source is playing. Room caps and quiet hours apply, and undo_last restores previous playback. Preserves shuffle/repeat; do not automatically replay a failed request."
)]
fn play_queue(
    ctx: &McpContext,
    zone: String,
    position: u32,
    expected_uri: Option<String>,
    expected_revision: Option<String>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    ctx.checkpoint()?;
    let backend = tools::backend().map_err(|failure| tool_error(&failure))?;
    let result = backend.play_queue(&fsonos_api::surface::queue::QueuePlayRequest {
        zone,
        position,
        expected_uri,
        expected_revision,
    })?;
    Ok(CompleteResult::new(result, ResultMeta::empty()))
}

/// Remove selected consecutive queue entries through the shared backend.
#[tool(
    description = "Remove consecutive entries from a room's group queue. First read get_queue, then pass the one-based starting position and exact renderer URIs in expected_uris (one to 100, in queue order). Pass that response's revision as expected_revision to reject intervening queue or coordinator changes. Uses revision-checked deletion, never clears/rebuilds the queue, and never automatically retries a failed write. This deletes queue entries and is NOT undoable with undo_last; read the queue again after any uncertain result rather than blindly retrying."
)]
fn remove_from_queue(
    ctx: &McpContext,
    zone: String,
    position: u32,
    expected_uris: Vec<String>,
    expected_revision: Option<String>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    ctx.checkpoint()?;
    let backend = tools::backend().map_err(|failure| tool_error(&failure))?;
    let result = backend.remove_from_queue(&fsonos_api::surface::queue::QueueRemoveRequest {
        zone,
        position,
        expected_uris,
        expected_revision,
    })?;
    Ok(CompleteResult::new(result, ResultMeta::empty()))
}

/// Rearrange an identified block without rebuilding the queue or playback.
#[tool(
    description = "Move consecutive queue entries without clearing, re-enqueueing or restarting playback. First read get_queue. Pass its revision as REQUIRED expected_revision, one-based position, and one to 100 exact consecutive renderer URIs in expected_uris. insert_before refers to a position in the ORIGINAL queue; total plus one appends. Destinations inside or just after the selected block are no-ops. Returns the block's new position and observed revision. Not undoable with undo_last. Never automatically retry a failed move or discard a revision; read the queue again after uncertainty."
)]
fn move_in_queue(
    ctx: &McpContext,
    zone: String,
    position: u32,
    insert_before: u32,
    expected_uris: Vec<String>,
    expected_revision: String,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    ctx.checkpoint()?;
    let backend = tools::backend().map_err(|failure| tool_error(&failure))?;
    let result = backend.move_in_queue(&fsonos_api::surface::queue::QueueMoveRequest {
        zone,
        position,
        insert_before,
        expected_uris,
        expected_revision,
    })?;
    Ok(CompleteResult::new(result, ResultMeta::empty()))
}

/// Build the FrankenSonos MCP server. `auto` admission accepts both the
/// 2024-11-05 and 2026-07-28 protocol eras, pinning each connection to the era
/// its client opened with.
#[must_use]
pub fn server() -> fastmcp::auto::Server {
    server_answering(Vec::new())
}

/// [`server`], whose HTTP listener answers only requests whose `Host` is one
/// of `hosts` (`fsonos_api::WebPolicy::hosts`): a web page that rebinds its
/// own DNS name onto the listener's address is refused (403) before any tool
/// runs. Empty answers any host.
#[must_use]
pub fn server_answering(hosts: Vec<String>) -> fastmcp::auto::Server {
    let mut http = fastmcp::HttpServerConfig::new();
    http.allowed_hosts = hosts;
    http.handler_config.max_body_size = fsonos_api::surface::announce::MAX_ANNOUNCE_REQUEST_BYTES;
    http.tool_input_max_bytes.insert(
        "announce".into(),
        fsonos_api::surface::announce::MAX_ANNOUNCE_REQUEST_BYTES,
    );
    fastmcp::auto::server_builder("fsonos", env!("CARGO_PKG_VERSION"))
        .http_config(http)
        .request_timeout(fsonos_api::surface::announce::ANNOUNCE_TIMEOUT_SECS)
        .tool(Echo)
        .tool(tools::ListZones)
        .tool(tools::ListRooms)
        .tool(tools::ListPlayers)
        .tool(tools::GetZoneState)
        .tool(tools::GetZone)
        .tool(tools::ListFavorites)
        .tool(tools::PlayFavorite)
        .tool(tools::RecentActions)
        .tool(tools::UndoLast)
        .tool(reviewed_undo::PreviewUndo)
        .tool(reviewed_undo::UndoChecked)
        .tool(tools::GetPolicy)
        .tool(tools::Events)
        .tool(tools::Doctor)
        .tool(tools::Play)
        .tool(tools::Pause)
        .tool(tools::Resume)
        .tool(tools::Next)
        .tool(tools::Previous)
        .tool(tools::Seek)
        .tool(tools::SetVolume)
        .tool(tools::MuteTool)
        .tool(tools::GetEq)
        .tool(tools::SetEq)
        .tool(tools::Group)
        .tool(tools::Ungroup)
        .tool(tools::MovePlayback)
        .tool(tools::GroupAll)
        .tool(tools::ListScenes)
        .tool(tools::GetScene)
        .tool(tools::SaveScene)
        .tool(tools::ApplyScene)
        .tool(tools::DeleteScene)
        .tool(tools::DjStart)
        .tool(tools::DjSkip)
        .tool(tools::DjStop)
        .tool(tools::DjSteer)
        .tool(tools::DjStatus)
        .tool(tools::DjMoods)
        .tool(tools::SearchLibrary)
        .tool(tools::SearchAndPlay)
        .tool(tools::RecentPlays)
        .tool(tools::GetQueue)
        .tool(tools::AddToQueue)
        .tool(PlayQueue)
        .tool(RemoveFromQueue)
        .tool(MoveInQueue)
        .tool(play_mode::GetPlayMode)
        .tool(play_mode::SetPlayMode)
        .tool(tools::Announce)
        .tool(tools::dj_feedback::DjFeedback)
        .tool(tools::dj_prefs::DjPreferences)
        .tool(tools::dj_prefs::DjPrefer)
        .tool(tools::dj_sync::DjSyncStatus)
        .tool(tools::dj_sync::DjSync)
        .tool(tools::schedules::FadeVolume)
        .tool(tools::schedules::FadeAndPause)
        .tool(tools::schedules::SetSleepTimer)
        .tool(tools::schedules::ListSleepTimers)
        .tool(tools::schedules::ListSchedules)
        .tool(tools::schedules::AddSchedule)
        .tool(tools::schedules::PauseSchedule)
        .tool(tools::schedules::ResumeSchedule)
        .tool(tools::schedules::RemoveSchedule)
        .resource(resources::ZonesResource)
        .resource(resources::ZoneResource)
        .resource(resources::DjResource)
        .build()
}

/// The tool error for `failure`: the `CODE: detail. Hint: ...` text an agent
/// reads (see `docs/ERRORS.md`). Both protocol eras deliver it as a tool
/// result with `isError` set; under 2026-07-28 its structured content is the
/// error body HTTP answers (code, detail, hint, suggestions, retryable), the
/// error's data here.
#[must_use]
pub fn tool_error(failure: &Failure) -> McpError {
    match serde_json::to_value(failure.body()) {
        Ok(body) => McpError::with_data(
            fastmcp::McpErrorCode::ToolExecutionError,
            failure.tool_text(),
            body,
        ),
        Err(_) => McpError::tool_error(failure.tool_text()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_api::ErrorCode;
    use fsonos_api::surface::announce::MAX_ANNOUNCE_REQUEST_BYTES;

    fn announce_input_limit_builder(tool: &str, maximum: usize) -> fastmcp_server::ServerBuilder {
        let mut http = fastmcp::HttpServerConfig::new();
        http.handler_config.max_body_size = MAX_ANNOUNCE_REQUEST_BYTES;
        http.tool_input_max_bytes.insert(tool.into(), maximum);
        fastmcp_server::ServerBuilder::try_new_with_fixed_protocol_policy(
            "announcement-limit-test",
            "1.0",
            fastmcp::ProtocolPolicy::Auto,
        )
        .unwrap()
        .http_config(http)
        .tool(tools::Announce)
    }

    fn assert_input_limit_refused(tool: &str, maximum: usize, reason: &str) {
        let result = announce_input_limit_builder(tool, maximum).try_build();
        let Err(fastmcp_server::ServerBuildError::InvalidConfiguration(refused)) = result else {
            panic!("invalid input budget for {tool} unexpectedly built a server");
        };
        assert_eq!(refused.len(), 1, "{refused:?}");
        assert_eq!(refused[0].kind, fastmcp_server::RegistrationKind::Tool);
        assert_eq!(refused[0].name, tool);
        assert!(refused[0].reason.contains(reason), "{refused:?}");
    }

    #[test]
    fn registered_announce_accepts_the_validated_upload_budget() {
        announce_input_limit_builder("announce", MAX_ANNOUNCE_REQUEST_BYTES)
            .try_build()
            .expect("the registered announcement tool supports its bounded upload budget");
    }

    #[test]
    fn zero_tool_input_budget_prevents_server_startup() {
        assert_input_limit_refused("announce", 0, "nonzero");
    }

    #[test]
    fn tool_input_budget_cannot_exceed_the_http_body_budget() {
        assert_input_limit_refused(
            "announce",
            MAX_ANNOUNCE_REQUEST_BYTES + 1,
            "fit the HTTP body budget",
        );
    }

    #[test]
    fn input_budget_for_an_unknown_tool_prevents_server_startup() {
        assert_input_limit_refused(
            "missing-tool",
            MAX_ANNOUNCE_REQUEST_BYTES,
            "unregistered tool",
        );
    }

    #[test]
    fn failures_render_as_tool_errors() {
        let failure = Failure::new(ErrorCode::UnknownRoom, "unknown room \"Kichen\"")
            .with_suggestions(["Kitchen@S1"]);
        let err = tool_error(&failure);
        assert_eq!(err.code, fastmcp::McpErrorCode::ToolExecutionError);
        assert_eq!(
            err.message,
            "UNKNOWN_ROOM: unknown room \"Kichen\". Hint: Use a suggested room, or list rooms \
             with list_zones (GET /zones). Did you mean: Kitchen@S1?"
        );
        // The body HTTP answers, for the 2026-07-28 structured content.
        assert_eq!(
            err.data,
            Some(serde_json::to_value(failure.body()).unwrap()),
            "{err:?}"
        );
        assert_eq!(err.data.as_ref().unwrap()["code"], "UNKNOWN_ROOM");
        assert_eq!(err.data.as_ref().unwrap()["retryable"], false);
    }

    #[test]
    fn normalizes_curly_apostrophe() {
        assert_eq!(normalize_room("Ada\u{2019}s Studio"), "ada's studio");
    }
}
