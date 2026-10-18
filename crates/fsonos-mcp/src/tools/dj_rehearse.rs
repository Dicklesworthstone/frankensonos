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
    description = "The set the DJ would play in the group a room plays in, from a given time, planned offline: each work with when it would start, its length, why the DJ would choose it (a summary and the weighted factors), its energy against the energy aimed for, and the mood steering it; then hour by hour, how varied the artists and genres are and how the energy follows the target. It plays, queues and records nothing. `works` (1-500, default 12) or `hours` (1-48) sets the length; `at` is HH:MM (its next time) or an RFC 3339 time (default now); `mood` and the constraint fields of dj_steer (include_/exclude_ artists, genres, composers and keywords, periods, decades, min/max minutes, energy_bias, allow_long) rehearse as if the group were steered so, in place of its steering and the time-of-day program; the same `seed` gives the same set. Use it to answer 'what would the DJ play at 11 tonight?'.",
    annotations(read_only, idempotent)
)]
#[allow(clippy::too_many_arguments)] // dj_steer's steering fields, as agents call them
async fn dj_rehearse(
    _ctx: &McpContext,
    zone: String,
    works: Option<u32>,
    hours: Option<u32>,
    at: Option<String>,
    mood: Option<String>,
    include_composers: Option<Vec<String>>,
    exclude_composers: Option<Vec<String>>,
    include_artists: Option<Vec<String>>,
    exclude_artists: Option<Vec<String>>,
    include_genres: Option<Vec<String>>,
    exclude_genres: Option<Vec<String>>,
    periods: Option<Vec<String>>,
    exclude_periods: Option<Vec<String>>,
    decades: Option<Vec<u16>>,
    exclude_decades: Option<Vec<u16>>,
    include_keywords: Option<Vec<String>>,
    exclude_keywords: Option<Vec<String>>,
    min_minutes: Option<u32>,
    max_minutes: Option<u32>,
    energy_bias: Option<i8>,
    allow_long: Option<bool>,
    seed: Option<u64>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    let req = DjRehearseRequest {
        zone,
        works,
        hours,
        at,
        mood,
        constraints: SteerConstraints {
            include_composers: include_composers.unwrap_or_default(),
            exclude_composers: exclude_composers.unwrap_or_default(),
            include_artists: include_artists.unwrap_or_default(),
            exclude_artists: exclude_artists.unwrap_or_default(),
            include_genres: include_genres.unwrap_or_default(),
            exclude_genres: exclude_genres.unwrap_or_default(),
            periods: periods.unwrap_or_default(),
            exclude_periods: exclude_periods.unwrap_or_default(),
            decades: decades.unwrap_or_default(),
            exclude_decades: exclude_decades.unwrap_or_default(),
            include_keywords: include_keywords.unwrap_or_default(),
            exclude_keywords: exclude_keywords.unwrap_or_default(),
            min_work_minutes: min_minutes,
            max_work_minutes: max_minutes,
            energy_bias: energy_bias.unwrap_or(0),
            allow_long_works: allow_long.unwrap_or(false),
        },
        seed,
    };
    with_backend_off_thread(move |b| b.dj_rehearse(&req)).await
}
