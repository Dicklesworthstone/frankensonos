//! The library refresh routes (see [`crate::surface::dj_sync`]).
//!
//! | Route | Body | Answer |
//! |---|---|---|
//! | `GET /dj/sync` | | [`LibrarySyncDto`] |
//! | `POST /dj/sync` | `{}` | [`LibrarySyncDto`]: the refresh started (or already running) |

use fastapi::core::RouteEntry;
use serde_json::{Map, Value};

use super::{Ctx, DJ, Op, answer, body_or_default};
use crate::surface::dj_sync::LibrarySyncDto;

/// Both routes.
pub(super) fn routes(cx: &Ctx<'_>) -> Vec<RouteEntry> {
    vec![
        cx.route(
            &Op::get(
                "/dj/sync",
                "dj_sync_status",
                DJ,
                "Where the DJ's library refresh from Spotify stands",
            ),
            |s, c, _| answer(s.dj_sync_status(c)),
        )
        .response_schema::<LibrarySyncDto>(200, "Running or not, and the last refresh"),
        cx.route(
            &Op::post(
                "/dj/sync",
                "dj_sync",
                DJ,
                "Refresh the DJ's library from Spotify now, in the background",
            ),
            |s, c, req| {
                answer(body_or_default(req, Map::<String, Value>::new()).and_then(|_| s.dj_sync(c)))
            },
        )
        .response_schema::<LibrarySyncDto>(200, "The refresh started (or already running)"),
    ]
}
