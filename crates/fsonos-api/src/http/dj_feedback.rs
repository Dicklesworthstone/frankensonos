//! `POST /dj/feedback` (see [`crate::surface::dj_feedback`]).

use fastapi::core::RouteEntry;

use super::{Ctx, DJ, Op, answer, body};
use crate::surface::dj_feedback::{DjFeedbackDto, DjFeedbackRequest};

/// The feedback route.
pub(super) fn routes(cx: &Ctx<'_>) -> Vec<RouteEntry> {
    vec![
        cx.route(
            &Op::post(
                "/dj/feedback",
                "dj_feedback",
                DJ,
                "Like or dislike the work playing (its composer and performer too), so the DJ plays them more or less",
            ),
            |s, c, req| answer(body::<DjFeedbackRequest>(req).and_then(|r| s.dj_feedback(c, &r))),
        )
        .request_schema::<DjFeedbackRequest>(true)
        .response_schema::<DjFeedbackDto>(200, "What was recorded"),
    ]
}
