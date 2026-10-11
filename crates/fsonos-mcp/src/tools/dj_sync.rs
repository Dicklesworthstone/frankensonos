//! The `dj_sync` and `dj_sync_status` tools (see
//! `fsonos_api::surface::dj_sync`).

use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult};
use fsonos_api::surface::dj_sync::LibrarySyncDto;

use super::{Backend, respond, with_backend};

/// A library refresh's state, said.
fn done(state: &LibrarySyncDto) -> String {
    state.done.clone()
}

impl Backend {
    /// The `dj_sync_status` tool: the running daemon's refresh when there is
    /// one (the stdio server has no Spotify library of its own).
    pub fn dj_sync_status(&self) -> McpResult<FinalCallToolResult> {
        if let Some(answer) = self.on_daemon("GET", "/dj/sync", None, done) {
            return answer;
        }
        respond(|| {
            let state = self.surface.dj_sync_status(&self.client)?;
            Ok((state.done.clone(), state))
        })
    }

    /// The `dj_sync` tool, on the running daemon when there is one.
    pub fn dj_sync(&self) -> McpResult<FinalCallToolResult> {
        let body = serde_json::json!({});
        if let Some(answer) = self.on_daemon("POST", "/dj/sync", Some(&body), done) {
            return answer;
        }
        respond(|| {
            let state = self.surface.dj_sync(&self.client)?;
            Ok((state.done.clone(), state))
        })
    }
}

#[tool(
    description = "Where the DJ's library refresh from Spotify stands: whether one is running, when the last one finished and what it found (tracks, how many the DJ can play, how many classical), and why the latest attempt failed if it did. The daemon refreshes daily on its own.",
    annotations(read_only, idempotent)
)]
fn dj_sync_status(_ctx: &McpContext) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend(Backend::dj_sync_status)
}

#[tool(
    description = "Refresh the DJ's library from the owner's Spotify now (read-only on Spotify), in the background: new saves and likes, their artists' genres, and the taste signals the sign-in allows. Use it after the owner saves new music; the DJ plays from what it finds from its next pick. Answers at once; call dj_sync_status to see when it is done. Starting one while one runs changes nothing.",
    annotations(idempotent)
)]
fn dj_sync(_ctx: &McpContext) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend(Backend::dj_sync)
}
