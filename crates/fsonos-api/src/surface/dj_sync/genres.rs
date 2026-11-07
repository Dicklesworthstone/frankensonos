//! Current artist genre enrichment, independent of the last full library read.

use fastapi::{JsonSchema, fastapi_openapi};
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

use super::timestamp_text;

/// The bounded genre worker's latest durable progress. An empty genre list
/// returned by Spotify is a completed lookup; it does not remain pending.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GenreEnrichmentDto {
    /// Artists whose genre lookup is still unfinished for the cached library.
    pub pending: usize,
    /// Artist lookups completed by the latest bounded pass, not a lifetime total.
    pub completed: usize,
    /// The current library worker is performing a genre-only pass.
    pub running: bool,
    /// Earliest next bounded pass after progress backoff, in Unix seconds.
    /// A later `not_before` deadline takes precedence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_at: Option<i64>,
    /// Spotify's saved read cooldown deadline, in Unix seconds. Manual sync
    /// requests also respect it; they do not bypass a server rate limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_before: Option<i64>,
    /// The cached library must be refreshed before genre continuation is safe,
    /// for example after the Spotify account or library generation changes.
    pub requires_refresh: bool,
}

impl GenreEnrichmentDto {
    /// Pending work, current activity, and server cooldowns in plain language.
    #[must_use]
    pub fn text(&self) -> String {
        let lookups = |count| {
            format!(
                "{count} artist genre lookup{}",
                if count == 1 { "" } else { "s" }
            )
        };
        let mut text = if self.requires_refresh {
            "Artist genre enrichment needs a full library refresh before it can continue.".into()
        } else if self.running {
            format!(
                "Enriching artist genres in the background: {} pending.",
                lookups(self.pending)
            )
        } else if self.pending == 0 {
            "Artist genre enrichment is complete.".into()
        } else {
            format!(
                "Artist genre enrichment is incomplete: {} pending.",
                lookups(self.pending)
            )
        };
        if self.completed > 0 {
            let _ = write!(
                text,
                " The last pass completed {}.",
                lookups(self.completed)
            );
        }
        if !self.running {
            if let Some(at) = self.not_before {
                let _ = write!(
                    text,
                    " Spotify cooldown: no reads before {}.",
                    timestamp_text(at)
                );
            }
            if self.pending > 0 && !self.requires_refresh {
                if let Some(at) = self.retry_at.into_iter().chain(self.not_before).max() {
                    let _ = write!(
                        text,
                        " The next genre pass is eligible at {}.",
                        timestamp_text(at)
                    );
                } else {
                    text.push_str(" The daemon will continue in bounded background passes.");
                }
            }
        }
        text
    }
}
