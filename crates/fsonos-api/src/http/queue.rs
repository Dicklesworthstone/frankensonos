//! Adding to, removing from or playing a room's group's queue (see [`crate::surface::queue`]).
//!
//! | Route | Body | Answer |
//! |---|---|---|
//! | `POST /queue` | [`crate::surface::queue::QueueRequest`] | [`crate::surface::queue::QueueAddedDto`] |
//! | `POST /queue/remove` | [`crate::surface::queue::QueueRemoveRequest`] | [`crate::surface::queue::QueueRemovedDto`] |
//! | `POST /queue/play` | [`crate::surface::queue::QueuePlayRequest`] | [`crate::OutcomeDto`] |

use fastapi::core::RouteEntry;

use super::{CONTROL, Ctx, Op, answer, body};
use crate::OutcomeDto;
use crate::surface::queue::{
    ADD, MOVE, PLAY, QueueAddedDto, QueueMoveRequest, QueueMovedDto, QueuePlayRequest,
    QueueRemoveRequest, QueueRemovedDto, QueueRequest, REMOVE,
};

mod mode;
mod search;

/// Queue addition, playback and identity-checked removal routes.
pub(super) fn routes(cx: &Ctx<'_>) -> Vec<RouteEntry> {
    let mut routes = vec![
        cx.route(
            &Op::post(
                "/queue",
                ADD,
                CONTROL,
                "Queue a Spotify source or native household favorite at the end or next, without starting playback (undoable)",
            ),
            |s, c, req| answer(body::<QueueRequest>(req).and_then(|r| s.queue_request(c, &r))),
        )
        .request_schema::<QueueRequest>(true)
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
                "Remove identified consecutive queue entries once (undoable when a complete backup is saved)",
            ),
            |s, c, req| answer(body::<QueueRemoveRequest>(req).and_then(|r| s.queue_remove(c, &r))),
        )
        .request_schema::<QueueRemoveRequest>(true)
        .response_schema::<QueueRemovedDto>(200, "The selected entries were removed"),
        cx.route(
            &Op::post(
                "/queue/move",
                MOVE,
                CONTROL,
                "Move identified queue entries natively without restarting playback (undoable when a complete backup is saved)",
            ),
            |s, c, req| answer(body::<QueueMoveRequest>(req).and_then(|r| s.queue_move(c, &r))),
        )
        .request_schema::<QueueMoveRequest>(true)
        .response_schema::<QueueMovedDto>(200, "The queue order was confirmed"),
    ];
    routes.extend(mode::routes(cx));
    routes.extend(search::routes(cx));
    routes
}
