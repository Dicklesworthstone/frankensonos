//! The `dj_sync` and `dj_sync_status` tools (see
//! `fsonos_api::surface::dj_sync`).

use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult};
use fsonos_api::surface::dj_sync::LibrarySyncDto;

use super::{Backend, respond, with_backend_off_thread};

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
    description = "Where the DJ's Spotify library update stands: current activity, the last full refresh and its track counts, and any latest error. Artist genre enrichment has separate running, pending and completed counts, a retry deadline, Spotify cooldown, and whether a full refresh is required. A finished full refresh can still have genres pending; the daemon continues bounded genre passes independently and refreshes the library daily. This status read starts no work.",
    annotations(read_only, idempotent)
)]
async fn dj_sync_status(ctx: &McpContext) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend_off_thread(ctx, Backend::dj_sync_status).await
}

#[tool(
    description = "Request a background refresh of the DJ's library from the owner's Spotify (read-only on Spotify): new saves and likes, artist genres, and the taste signals the sign-in allows. Use it after the owner saves new music; newly selected DJ picks use the updated cache. Answers at once; use dj_sync_status to follow the full refresh and independent bounded genre enrichment. Existing work and Spotify cooldowns are respected; repeating this request does not bypass a rate limit.",
    annotations(idempotent)
)]
async fn dj_sync(ctx: &McpContext) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend_off_thread(ctx, Backend::dj_sync).await
}
