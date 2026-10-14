//! `GET /players`: every player, as `fsonos discover` lists them (see
//! [`crate::surface::players`]).

use fastapi::core::RouteEntry;

use super::{Ctx, DAEMON, Op, answer};
use crate::surface::players::{PlayerDto, TOOL};

/// The players route.
pub(super) fn routes(cx: &Ctx<'_>) -> Vec<RouteEntry> {
    vec![
        cx.route(
            &Op::get(
                "/players",
                TOOL,
                DAEMON,
                "Every player, by household and room",
            ),
            |surface, client, _| answer(surface.players(client)),
        )
        .response_schema::<Vec<PlayerDto>>(200, "The players"),
    ]
}
