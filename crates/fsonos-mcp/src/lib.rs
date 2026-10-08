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
    fastmcp::auto::server_builder("fsonos", env!("CARGO_PKG_VERSION"))
        .tool(Echo)
        .tool(tools::ListZones)
        .tool(tools::ListRooms)
        .tool(tools::GetZoneState)
        .tool(tools::ListFavorites)
        .tool(tools::PlayFavorite)
        .tool(tools::RecentActions)
        .tool(tools::UndoLast)
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
        .tool(tools::DjStart)
        .tool(tools::DjSkip)
        .tool(tools::DjStop)
        .tool(tools::SearchLibrary)
        .tool(tools::RecentPlays)
        .resource(resources::ZonesResource)
        .resource(resources::ZoneResource)
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
