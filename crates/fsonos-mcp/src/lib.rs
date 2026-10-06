//! FrankenSonos MCP server.
//!
//! Exposes the house audio to AI agents (Claude, Grok, Meta Muse, OpenAI "dots"
//! — anything speaking MCP) as a small set of well-described tools:
//! `list_zones`, `play`, `pause`, `resume`, `next`, `set_volume`, `group`,
//! `ungroup`, `dj_start`, `dj_skip`. Served over stdio and streamable HTTP via
//! `fastmcp_rust`. The `#[tool]` wiring lands in FND-DEPS; the tool argument /
//! result shapes and their pure validation live here.

use serde::{Deserialize, Serialize};

/// Arguments for the `play` tool.
#[derive(Debug, Serialize, Deserialize)]
pub struct PlayArgs {
    /// Room or zone name (matched case-insensitively, curly apostrophes
    /// normalized — real room names like "Jeff's Office" use U+2019).
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
        assert_eq!(normalize_room("Jeff\u{2019}s Office"), "jeff's office");
    }
}
