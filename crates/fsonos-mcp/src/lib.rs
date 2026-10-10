//! FrankenSonos MCP server.
//!
//! Exposes the house audio to AI agents (Claude, Grok, Meta Muse, OpenAI "dots"
//! — anything speaking MCP) as a small set of well-described tools:
//! `list_zones`, `play`, `pause`, `resume`, `next`, `set_volume`, `group`,
//! `ungroup`, `dj_start`, `dj_skip`. Served over stdio and streamable HTTP via
//! `fastmcp_rust`. [`server`] builds the server; [`tools`] holds the tools and
//! the [`tools::Backend`] of speakers they act on, installed once per process
//! with [`tools::install`]; [`resources`] serves the zones as documents. The tools take the HTTP API's request bodies
//! (`fsonos_api::request`) and share its validation, planning, execution and
//! house policy, so both surfaces answer alike.

pub mod resources;
pub mod tools;

use fastmcp::prelude::*;
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

/// Build the FrankenSonos MCP server. `auto` admission accepts both the
/// 2024-11-05 and 2026-07-28 protocol eras, pinning each connection to the era
/// its client opened with.
#[must_use]
pub fn server() -> fastmcp::auto::Server {
    let mut http = fastmcp::HttpServerConfig::new();
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
        .tool(tools::GetZoneState)
        .tool(tools::ListFavorites)
        .tool(tools::PlayFavorite)
        .tool(tools::RecentActions)
        .tool(tools::UndoLast)
        .tool(tools::GetPolicy)
        .tool(tools::Doctor)
        .tool(tools::Play)
        .tool(tools::Pause)
        .tool(tools::Resume)
        .tool(tools::Next)
        .tool(tools::Previous)
        .tool(tools::SetVolume)
        .tool(tools::MuteTool)
        .tool(tools::Group)
        .tool(tools::Ungroup)
        .tool(tools::MovePlayback)
        .tool(tools::GroupAll)
        .tool(tools::ListScenes)
        .tool(tools::SaveScene)
        .tool(tools::ApplyScene)
        .tool(tools::DjStart)
        .tool(tools::DjSkip)
        .tool(tools::DjStop)
        .tool(tools::DjSteer)
        .tool(tools::DjStatus)
        .tool(tools::DjMoods)
        .tool(tools::SearchLibrary)
        .tool(tools::RecentPlays)
        .tool(tools::Announce)
        .tool(tools::dj_feedback::DjFeedback)
        .tool(tools::dj_prefs::DjPreferences)
        .tool(tools::dj_prefs::DjPrefer)
        .tool(tools::dj_sync::DjSyncStatus)
        .tool(tools::dj_sync::DjSync)
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
/// result with `isError` set; the code travels in the text.
#[must_use]
pub fn tool_error(failure: &Failure) -> McpError {
    McpError::tool_error(failure.tool_text())
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
    }

    #[test]
    fn normalizes_curly_apostrophe() {
        assert_eq!(normalize_room("Ada\u{2019}s Studio"), "ada's studio");
    }
}
