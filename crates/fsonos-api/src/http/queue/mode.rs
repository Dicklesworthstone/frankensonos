//! Queue shuffle/repeat without starting or replacing playback.

use fastapi::core::RouteEntry;

use super::super::{CONTROL, Ctx, Op, ZONES, answer, body, path_room};
use crate::surface::queue::{MODE_READ, MODE_WRITE, PlayModeDto, PlayModeRequest, PlayModeSetDto};

pub(super) fn routes(cx: &Ctx<'_>) -> Vec<RouteEntry> {
    vec![
        cx.route(
            &Op::get(
                "/zones/{room}/play-mode",
                MODE_READ,
                ZONES,
                "Read the group's queue shuffle/repeat setting and DJ ownership",
            ),
            |s, c, req| answer(path_room(req).and_then(|room| s.get_play_mode(c, &room))),
        )
        .response_schema::<PlayModeDto>(200, "The observed queue mode"),
        cx.route(
            &Op::post(
                "/play-mode",
                MODE_WRITE,
                CONTROL,
                "Set queue shuffle/repeat without restarting playback (snapshot undo)",
            ),
            |s, c, req| answer(body::<PlayModeRequest>(req).and_then(|r| s.set_play_mode(c, &r))),
        )
        .request_schema::<PlayModeRequest>(true)
        .response_schema::<PlayModeSetDto>(200, "The confirmed queue mode"),
    ]
}
