//! FrankenSonos MCP server.
//!
//! Exposes the house audio to AI agents (Claude, Grok, Meta Muse, OpenAI "dots"
//! — anything speaking MCP) as a small set of well-described tools:
//! `list_zones`, `play`, `pause`, `resume`, `next`, `set_volume`, `group`,
//! `ungroup`, `dj_start`, `dj_skip`. Served over stdio and streamable HTTP via
//! `fastmcp_rust`. [`server`] builds the server; the tool argument / result
//! shapes and their pure validation live here.

use fastmcp::prelude::*;
use serde::{Deserialize, Serialize};

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

/// Arguments for the `play` tool.
#[derive(Debug, Serialize, Deserialize)]
pub struct PlayArgs {
    /// Room or zone name (matched case-insensitively, curly apostrophes
    /// normalized — room names such as "Ada's Studio" often use U+2019).
    pub zone: String,
    pub source_uri: String,
}

/// Normalize a room name for matching: lowercase and fold curly apostrophes to
/// a straight one. Room names from Sonos commonly use U+2019.
#[must_use]
pub fn normalize_room(name: &str) -> String {
    name.trim()
        .chars()
        .map(|c| match c {
            '\u{2019}' | '\u{2018}' | '\u{201B}' => '\'',
            other => other.to_ascii_lowercase(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_curly_apostrophe() {
        assert_eq!(normalize_room("Ada\u{2019}s Studio"), "ada's studio");
    }
}
