//! Adding to, removing from or playing a room's group's queue (see [`crate::surface::queue`]).
//!
//! | Route | Body | Answer |
//! |---|---|---|
//! | `POST /queue` | [`crate::surface::queue::QueueAddRequest`] | [`crate::surface::queue::QueueAddedDto`] |
//! | `POST /queue/remove` | [`crate::surface::queue::QueueRemoveRequest`] | [`crate::surface::queue::QueueRemovedDto`] |
//! | `POST /queue/play` | [`crate::surface::queue::QueuePlayRequest`] | [`crate::OutcomeDto`] |

use fastapi::core::RouteEntry;

use super::{CONTROL, Ctx, Op, answer, body};
use crate::OutcomeDto;
use crate::surface::queue::{
    ADD, PLAY, REMOVE, QueueAddRequest, QueueAddedDto, QueuePlayRequest, QueueRemoveRequest,
    QueueRemovedDto,
};

/// Queue addition, playback and identity-checked removal routes.
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
        cx.route(
            &Op::post(
                "/queue/play",
                PLAY,
                CONTROL,
                "Play an existing queue position without adding tracks (policy-capped, undoable)",
            ),
            |s, c, req| answer(body::<QueuePlayRequest>(req).and_then(|r| s.queue_play(c, &r))),
        )
        .request_schema::<QueuePlayRequest>(true)
        .response_schema::<OutcomeDto>(200, "The selected queue entry is playing"),
        cx.route(
            &Op::post(
                "/queue/remove",
                REMOVE,
                CONTROL,
                "Remove identified consecutive queue entries once (not undoable)",
            ),
            |s, c, req| answer(body::<QueueRemoveRequest>(req).and_then(|r| s.queue_remove(c, &r))),
        )
        .request_schema::<QueueRemoveRequest>(true)
        .response_schema::<QueueRemovedDto>(200, "The selected entries were removed"),
    ]
}
