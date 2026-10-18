//! The `dj_rehearse` tool (see `fsonos_api::surface::dj_rehearse`).

use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult};
use fsonos_api::dj::SteerConstraints;
use fsonos_api::surface::dj_rehearse::{DjRehearsalDto, DjRehearseRequest};

use super::{Backend, respond, with_backend_off_thread};

impl Backend {
    /// The `dj_rehearse` tool, on the running daemon when there is one: its
    /// DJ knows what it has queued ahead.
    pub fn dj_rehearse(&self, req: &DjRehearseRequest) -> McpResult<FinalCallToolResult> {
        let body = serde_json::to_value(req).ok();
        if let Some(answer) =
            self.on_daemon("POST", "/dj/rehearse", body.as_ref(), DjRehearsalDto::text)
        {
            return answer;
        }
        respond(|| {
            let set = self.surface.dj_rehearse(&self.client, req)?;
            Ok((set.text(), set))
        })
    }
}

#[tool(
    description = "The set the DJ would play in the group a room plays in, from a given time, planned offline: each work with when it would start, its length, why the DJ would choose it (a summary and the weighted factors), its energy against the energy aimed for, and the mood steering it; then hour by hour, how varied the artists and genres are and how the energy follows the target. It plays, queues and records nothing. `works` (1-500, default 12) or `hours` (1-48) sets the length; `at` is HH:MM (its next time) or an RFC 3339 time (default now); `mood` rehearses as if the group were steered by that mood (dj_moods lists them); the same `seed` gives the same set. Use it to answer 'what would the DJ play at 11 tonight?'.",
    annotations(read_only, idempotent)
)]
async fn dj_rehearse(
    _ctx: &McpContext,
    zone: String,
    works: Option<u32>,
    hours: Option<u32>,
    at: Option<String>,
    mood: Option<String>,
    seed: Option<u64>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    let req = DjRehearseRequest {
        zone,
        works,
        hours,
        at,
        mood,
        constraints: SteerConstraints::default(),
        seed,
    };
    with_backend_off_thread(move |b| b.dj_rehearse(&req)).await
}
