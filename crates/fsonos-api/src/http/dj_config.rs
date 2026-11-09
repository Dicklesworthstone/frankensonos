//! Read and atomically edit owner-wide moods and daily programs.

use super::{Ctx, DJ, Op, answer, body};
use crate::surface::dj_config::{DjConfigDto, DjConfigureRequest, DjConfiguredDto};
use fastapi::core::RouteEntry;

pub(super) fn routes(cx: &Ctx<'_>) -> Vec<RouteEntry> {
    vec![
        cx.route(&Op::get("/dj/configuration", "dj_configuration", DJ,
            "Reusable DJ moods and automatic daily programs, with the exact editable revision"),
            |s, c, _| answer(s.dj_configuration(c)))
            .response_schema::<DjConfigDto>(200, "Editable overrides and effective configuration"),
        cx.route(&Op::post("/dj/configuration", "dj_configure", DJ,
            "Replace owner-wide DJ moods and programs only at the revision read; applies from the next pick"),
            |s, c, req| answer(body::<DjConfigureRequest>(req).and_then(|b| s.dj_configure(c, &b))))
            .request_schema::<DjConfigureRequest>(true)
            .response_schema::<DjConfiguredDto>(200, "Saved configuration and new revision"),
    ]
}
