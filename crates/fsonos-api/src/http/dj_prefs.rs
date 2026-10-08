//! The standing DJ preferences routes (see [`crate::surface::dj_prefs`]).
//!
//! | Route | Body | Answer |
//! |---|---|---|
//! | `GET /dj/preferences` | | [`PreferencesDto`] |
//! | `POST /dj/preferences` | [`DjPreferRequest`] | [`PreferredDto`]: what changed, and the preferences now |

use fastapi::core::RouteEntry;

use super::{Ctx, DJ, Op, answer, body};
use crate::surface::dj_prefs::{DjPreferRequest, PreferencesDto, PreferredDto};

/// Both preferences routes.
pub(super) fn routes(cx: &Ctx<'_>) -> Vec<RouteEntry> {
    vec![
        cx.route(
            &Op::get(
                "/dj/preferences",
                "dj_preferences",
                DJ,
                "The owner's standing DJ preferences, for every group",
            ),
            |s, c, _| answer(s.dj_preferences(c)),
        )
        .response_schema::<PreferencesDto>(200, "The preferences"),
        cx.route(
            &Op::post(
                "/dj/preferences",
                "dj_prefer",
                DJ,
                "Set or unset one standing DJ preference; it applies from the next pick",
            ),
            |s, c, req| answer(body::<DjPreferRequest>(req).and_then(|b| s.dj_prefer(c, &b))),
        )
        .request_schema::<DjPreferRequest>(true)
        .response_schema::<PreferredDto>(200, "What changed, and the preferences now"),
    ]
}
