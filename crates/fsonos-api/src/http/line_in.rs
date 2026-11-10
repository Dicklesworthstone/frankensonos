//! Discover native line-in sources and select one through shared playback.

use fastapi::core::RouteEntry;
use fastapi::{JsonSchema, Request, fastapi_openapi};

use super::{Ctx, Op, answer, body, query_param};
use crate::{Failure, LineInSourcesDto, OutcomeDto, PlayLineInRequest};

pub(super) fn routes(cx: &Ctx<'_>) -> Vec<RouteEntry> {
    vec![
        cx.route(
            &Op::get("/line-in", "list_line_in", "line-in",
                "Discover advertised native inputs in the destination household (list_players permission; cable and signal state unknown)"),
            |surface, client, request| answer(query(request).and_then(|q| surface.line_in_sources(client, &q.zone))),
        )
        .query_schema::<InputQuery>(true)
        .response_schema::<LineInSourcesDto>(200, "Native input identities and any players that could not be read"),
        cx.route(
            &Op::post("/line-in/play", "play_line_in", "line-in",
                "Play a native line-in on a group in the same household while retaining its queue (play permission)"),
            |surface, client, request| answer(body::<PlayLineInRequest>(request).and_then(|r| surface.play_line_in(client, &r))),
        )
        .request_schema::<PlayLineInRequest>(true)
        .response_schema::<OutcomeDto>(200, "The native input was selected for playback"),
    ]
}

#[derive(JsonSchema)]
struct InputQuery {
    /// Destination room or alias; qualify duplicate names as Room@S1 / Room@S2.
    zone: String,
}

fn query(request: &Request) -> Result<InputQuery, Failure> {
    let raw = request.query().unwrap_or_default();
    if raw.len() > 4096 || raw.split('&').filter(|part| !part.is_empty()).count() != 1 {
        return Err(Failure::invalid(
            "pass exactly one zone parameter to GET /line-in",
        ));
    }
    let zone = query_param(request, "zone")?
        .ok_or_else(|| Failure::invalid("name the destination: GET /line-in?zone=<room>"))?;
    Ok(InputQuery { zone })
}
