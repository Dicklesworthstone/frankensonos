//! A room's tone (see [`crate::surface::eq`]).
//!
//! | Route | Body | Answer |
//! |---|---|---|
//! | `GET /zones/{room}/eq` | | [`crate::surface::eq::EqDto`] |
//! | `POST /eq` | [`crate::surface::eq::EqRequest`] | [`crate::surface::eq::EqSetDto`] |

use fastapi::core::RouteEntry;

use super::{CONTROL, Ctx, Op, ZONES, answer, body, path_room};
use crate::surface::eq::{self, EqDto, EqRequest, EqSetDto};

/// The tone read and change.
pub(super) fn routes(cx: &Ctx<'_>) -> Vec<RouteEntry> {
    vec![
        cx.route(
            &Op::get(
                "/zones/{room}/eq",
                eq::READ,
                ZONES,
                "A room's bass, treble and loudness",
            ),
            |s, c, req| answer(path_room(req).and_then(|room| s.get_eq(c, &room))),
        )
        .response_schema::<EqDto>(200, "The room's tone"),
        cx.route(
            &Op::post(
                "/eq",
                eq::WRITE,
                CONTROL,
                "Change a room's bass, treble or loudness (undoable)",
            ),
            |s, c, req| answer(body::<EqRequest>(req).and_then(|r| s.set_eq(c, &r))),
        )
        .request_schema::<EqRequest>(true)
        .response_schema::<EqSetDto>(200, "What changed, and the room's tone now"),
    ]
}
