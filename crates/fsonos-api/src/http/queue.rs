//! Adding to a room's group's queue (see [`crate::surface::queue`]).
//!
//! | Route | Body | Answer |
//! |---|---|---|
//! | `POST /queue` | [`crate::surface::queue::QueueAddRequest`] | [`crate::surface::queue::QueueAddedDto`] |

use fastapi::core::RouteEntry;

use super::{CONTROL, Ctx, Op, answer, body};
use crate::surface::queue::{ADD, QueueAddRequest, QueueAddedDto};

/// The queue add route.
pub(super) fn routes(cx: &Ctx<'_>) -> Vec<RouteEntry> {
    vec![
        cx.route(
            &Op::post(
                "/queue",
                ADD,
                CONTROL,
                "Add a Spotify track, album or playlist to a room's group's queue, at its end or next (undoable)",
            ),
            |s, c, req| answer(body::<QueueAddRequest>(req).and_then(|r| s.queue_add(c, &r))),
        )
        .request_schema::<QueueAddRequest>(true)
        .response_schema::<QueueAddedDto>(200, "Where the tracks went"),
    ]
}
