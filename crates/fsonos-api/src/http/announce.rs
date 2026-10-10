//! `POST /announce`: speak, chime or upload a WAV, then put the music back (see
//! [`crate::surface::announce`]).

use fastapi::core::RouteEntry;

use super::{CONTROL, Ctx, Op, answer, body};
use crate::surface::announce::{AnnounceDto, AnnounceRequest, TOOL};

/// The announcement route.
pub(super) fn routes(cx: &Ctx<'_>) -> Vec<RouteEntry> {
    vec![
        cx.route(
            &Op::post(
                "/announce",
                TOOL,
                CONTROL,
                "Speak, chime or play an uploaded WAV in rooms, then put the music back",
            ),
            |surface, client, req| {
                answer(body::<AnnounceRequest>(req).and_then(|r| surface.announce(client, &r)))
            },
        )
        .request_schema::<AnnounceRequest>(true)
        .response_schema::<AnnounceDto>(200, "What each household heard, and what was put back"),
    ]
}
