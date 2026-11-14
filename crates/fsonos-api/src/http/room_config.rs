//! Read and edit the daemon's persisted room aliases and default targets.

use fastapi::core::RouteEntry;

use super::{Ctx, DAEMON, Op, answer, body};
use crate::surface::room_config::{RoomConfigurationDto, RoomConfigureRequest, RoomConfiguredDto};

pub(super) fn routes(cx: &Ctx<'_>) -> Vec<RouteEntry> {
    vec![
        cx.route(
            &Op::get(
                "/rooms/configuration",
                "room_configuration",
                DAEMON,
                "Every saved room alias and default, including offline targets, with its revision",
            ),
            |surface, client, _| answer(surface.room_configuration(client)),
        )
        .response_schema::<RoomConfigurationDto>(200, "The stored room configuration and advisory warnings"),
        cx.route(
            &Op::post(
                "/rooms/configuration",
                "configure_rooms",
                DAEMON,
                "Edit one room alias or default at its observed revision; requires owner or manage_others administration",
            ),
            |surface, client, request| {
                answer(body::<RoomConfigureRequest>(request).and_then(|request| surface.configure_rooms(client, &request)))
            },
        )
        .request_schema::<RoomConfigureRequest>(true)
        .response_schema::<RoomConfiguredDto>(200, "The accepted configuration, with no playback changes"),
    ]
}
