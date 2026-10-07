//! FrankenSonos MCP server.
//!
//! Exposes the house audio to AI agents (Claude, Grok, Meta Muse, OpenAI "dots"
//! — anything speaking MCP) as a small set of well-described tools:
//! `list_zones`, `play`, `pause`, `resume`, `next`, `set_volume`, `group`,
//! `ungroup`, `dj_start`, `dj_skip`. Served over stdio and streamable HTTP via
//! `fastmcp_rust`. [`server`] builds the server (today: an `echo` connectivity
//! tool). The control tools take the HTTP API's request bodies
//! (`fsonos_api::request`) and share its validation and planning, so both
//! surfaces answer alike.

use fastmcp::prelude::*;
use fastmcp::{ContentBlock, FinalCallToolResult};
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
        .build()
}

/// The tool-error result for `failure`: the `CODE: detail. Hint: ...` text an
/// agent reads, with the HTTP API's JSON error body as structured content (see
/// `docs/ERRORS.md`).
#[must_use]
pub fn failure_result(failure: &Failure) -> FinalCallToolResult {
    FinalCallToolResult {
        content: vec![ContentBlock::text(failure.tool_text())],
        is_error: true,
        structured_content: Some(
            serde_json::to_value(failure.body()).expect("ApiError serializes"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_api::ErrorCode;
    use serde_json::json;

    #[test]
    fn failures_render_as_tool_errors() {
        let failure = Failure::new(ErrorCode::UnknownRoom, "unknown room \"Kichen\"")
            .with_suggestions(["Kitchen@S1"]);
        let wire = serde_json::to_value(failure_result(&failure)).unwrap();
        assert_eq!(wire["isError"], true);
        assert_eq!(
            wire["content"][0]["text"],
            "UNKNOWN_ROOM: unknown room \"Kichen\". Hint: Use a suggested room, or list rooms \
             with list_zones (GET /zones). Did you mean: Kitchen@S1?"
        );
        assert_eq!(
            wire["structuredContent"],
            json!({
                "detail": "unknown room \"Kichen\"",
                "code": "UNKNOWN_ROOM",
                "hint": "Use a suggested room, or list rooms with list_zones (GET /zones).",
                "suggestions": ["Kitchen@S1"],
                "retryable": false
            })
        );
    }

    #[test]
    fn normalizes_curly_apostrophe() {
        assert_eq!(normalize_room("Ada\u{2019}s Studio"), "ada's studio");
    }
}
