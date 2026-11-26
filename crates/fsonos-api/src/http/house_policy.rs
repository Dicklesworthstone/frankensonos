//! The house-policy route (see [`crate::surface::house_policy`]).
//!
//! | Route | Body | Answer |
//! |---|---|---|
//! | `GET /policy` | | [`crate::surface::house_policy::PolicyDto`]: the effective policy, and the caller under it |

use fastapi::core::RouteEntry;

use super::{Ctx, DAEMON, LOG, Op, answer, body};
use crate::surface::house_policy::PolicyConfigureRequest;

/// The `GET /policy` route.
pub(super) fn routes(cx: &Ctx<'_>) -> Vec<RouteEntry> {
    vec![
        cx.route(
            &Op::get(
                "/policy",
                "get_policy",
                LOG,
                "The house policy in effect (limits, quiet hours, room caps, client rules) and who you are under it",
            ),
            |s, c, _| answer(s.policy_view(c)),
        )
        .response_schema::<serde_json::Value>(
            200,
            "The policy: `you`, `you_are_capped`, `defaults`, `quiet_hours`, `rooms`, `clients`",
        ),
        cx.route(
            &Op::get(
                "/policy/configuration",
                "policy_configuration",
                DAEMON,
                "The stored house policy and exact revision; requires owner or manage_others administration",
            ),
            |surface, client, _| answer(surface.policy_configuration(client)),
        )
        .response_schema::<serde_json::Value>(200, "Saved defaults, quiet hours, room limits, client rules and revision"),
        cx.route(
            &Op::post(
                "/policy/configuration",
                "configure_policy",
                DAEMON,
                "Edit one reviewed house policy rule at its saved revision; requires current house-config permission and owner or manage_others administration",
            ),
            |surface, client, request| {
                answer(body::<PolicyConfigureRequest>(request)
                    .and_then(|request| surface.configure_policy(client, &request)))
            },
        )
        .request_schema::<PolicyConfigureRequest>(true)
        .response_schema::<serde_json::Value>(200, "The accepted policy configuration; future commands use these rules"),
    ]
}
