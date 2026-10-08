//! The `dj_preferences` and `dj_prefer` tools (see
//! `fsonos_api::surface::dj_prefs`).

use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult};
use fsonos_api::surface::dj_prefs::DjPreferRequest;

use super::{Backend, respond, with_backend};

impl Backend {
    /// The `dj_preferences` tool.
    pub fn dj_preferences(&self) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let preferences = self.surface.dj_preferences(&self.client)?;
            Ok((preferences.text(), preferences))
        })
    }

    /// The `dj_prefer` tool.
    pub fn dj_prefer(&self, req: &DjPreferRequest) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let preferred = self.surface.dj_prefer(&self.client, req)?;
            Ok((preferred.done.clone(), preferred))
        })
    }
}

#[tool(
    description = "The owner's standing DJ preferences, which hold for every group's DJ until changed: genres, artists, eras and moods to favor or avoid, a default energy, whether explicit tracks may play, and artists, albums or tracks to pin or ban. Steering a group (dj_steer) outranks them; they outrank likes and dislikes.",
    annotations(read_only, idempotent)
)]
fn dj_preferences(_ctx: &McpContext) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend(Backend::dj_preferences)
}

#[tool(
    description = "Set or unset one standing DJ preference; it holds for every group until changed and applies from the DJ's next pick. Turn the owner's words into `key` and `value`. List keys add one item: favor.genres / avoid.genres (\"jazz\" finds \"cool jazz\"), favor.artists / avoid.artists, favor.eras / avoid.eras (a decade \"1960s\" or a classical period \"baroque\"), favor.moods / avoid.moods (dj_moods lists them), pin.artists / pin.albums / pin.tracks (always welcome, favored most) and ban.artists / ban.albums / ban.tracks (never; albums and tracks by Spotify URI or exact title). Plain keys: energy (0-100, in place of the time-of-day curve) and explicit (true or false). Examples: 'more jazz' is favor.genres \"jazz\"; 'never play Nickelback' is ban.artists \"Nickelback\"; 'go easy on the 80s' is avoid.eras \"1980s\"; 'keep it mellow' is energy \"30\". `unset` true removes `value` from a list (no value: the whole list; energy and explicit go back to their defaults). An avoid gives way only when nothing else is left; a ban never does."
)]
fn dj_prefer(
    _ctx: &McpContext,
    key: String,
    value: Option<String>,
    unset: Option<bool>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    let req = DjPreferRequest {
        key,
        value,
        unset: unset.unwrap_or(false),
    };
    with_backend(move |b| b.dj_prefer(&req))
}
