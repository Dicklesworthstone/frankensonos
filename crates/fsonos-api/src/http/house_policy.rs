//! The house-policy route (see [`crate::surface::house_policy`]).
//!
//! | Route | Body | Answer |
//! |---|---|---|
//! | `GET /policy` | | [`crate::surface::house_policy::PolicyDto`]: the effective policy, and the caller under it |

use fastapi::core::RouteEntry;

use super::{Ctx, LOG, Op, answer};

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
    ]
}
