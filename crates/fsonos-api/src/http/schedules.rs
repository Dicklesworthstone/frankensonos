//! The sleep-timer and schedule routes (see [`crate::surface::schedules`]).
//!
//! | Route | Body | Answer |
//! |---|---|---|
//! | `GET /sleep?zone=<target>` | | `[SleepTimerDto]`: the selected groups' timers, or every running one |
//! | `POST /sleep` | [`SleepRequest`] | [`SleepOutcome`]: one timer or complete per-group results |
//! | `GET /schedules` | | `[ScheduleDto]` |
//! | `POST /schedules` | [`ScheduleRequest`] | [`ScheduleDto`] |
//! | `POST /schedules/update` | [`UpdateScheduleRequest`] | [`ScheduleDto`]: revision-bound edit retaining the creator and enabled state |
//! | `POST /schedules/remove`, `/pause`, `/resume` | [`ScheduleIdRequest`] | [`ScheduleDto`] |
//! | `POST /volume/fade` | [`FadeVolumeRequest`] | [`crate::OutcomeDto`] |
//! | `POST /pause/fade` | [`FadePauseRequest`] | [`crate::OutcomeDto`] |

use fastapi::core::RouteEntry;
use fastapi::{JsonSchema, Request, fastapi_openapi};

use super::{CONTROL, Ctx, Op, answer, body, query_param};
use crate::execute::OutcomeDto;
use crate::failure::Failure;
use crate::surface::schedules::{
    FadePauseRequest, FadeVolumeRequest, ScheduleDto, ScheduleIdRequest, ScheduleRequest,
    SleepOutcome, SleepRequest, SleepTimerDto, UpdateScheduleRequest,
};

const SCHEDULES: &str = "schedules";

/// Every sleep-timer and schedule route.
pub(super) fn routes(cx: &Ctx<'_>) -> Vec<RouteEntry> {
    let mut routes = vec![
        cx.route(
            &Op::get(
                "/sleep",
                "list_sleep_timers",
                SCHEDULES,
                "The sleep timers that run in a room, saved room set, or every group",
            ),
            |s, c, req| answer(sleep_query(req).and_then(|q| s.sleep_timers(c, q.zone.as_deref()))),
        )
        .query_schema::<SleepQuery>(false)
        .response_schema::<Vec<SleepTimerDto>>(200, "The running timers"),
        cx.route(
            &Op::post(
                "/sleep",
                "set_sleep_timer",
                SCHEDULES,
                "Set, extend or cancel sleep timers in a room, saved room set or every group",
            ),
            |s, c, req| answer(body::<SleepRequest>(req).and_then(|r| s.set_sleep_timers(c, &r))),
        )
        .request_schema::<SleepRequest>(true)
        .response_schema::<SleepOutcome>(200, "One timer or every group's applied/failed receipt"),
        cx.route(
            &Op::get(
                "/schedules",
                "list_schedules",
                SCHEDULES,
                "Every schedule, with its next run",
            ),
            |s, c, _| answer(s.schedules(c)),
        )
        .response_schema::<Vec<ScheduleDto>>(200, "The schedules, by id"),
        cx.route(
            &Op::post(
                "/schedules",
                "add_schedule",
                SCHEDULES,
                "Schedule the DJ, a scene, pause, volume, speech or a chime, once or every week",
            ),
            |s, c, req| answer(body::<ScheduleRequest>(req).and_then(|r| s.add_schedule(c, &r))),
        )
        .request_schema::<ScheduleRequest>(true)
        .response_schema::<ScheduleDto>(200, "The schedule added"),
        cx.route(
            &Op::post(
                "/schedules/update",
                "update_schedule",
                SCHEDULES,
                "Edit a reviewed schedule's future runs while retaining its identity, creator and enabled state",
            ),
            |s, c, req| answer(body::<UpdateScheduleRequest>(req).and_then(|r| s.update_schedule(c, &r))),
        )
        .request_schema::<UpdateScheduleRequest>(true)
        .response_schema::<ScheduleDto>(200, "The confirmed edited schedule with its new revision"),
    ];
    for (path, id, summary, paused) in [
        (
            "/schedules/pause",
            "pause_schedule",
            "Pause a schedule: it does not run until resumed",
            Some(true),
        ),
        (
            "/schedules/resume",
            "resume_schedule",
            "Resume a paused schedule from its next time",
            Some(false),
        ),
        (
            "/schedules/remove",
            "remove_schedule",
            "Remove a schedule",
            None,
        ),
    ] {
        routes.push(
            cx.route(&Op::post(path, id, SCHEDULES, summary), move |s, c, req| {
                answer(body::<ScheduleIdRequest>(req).and_then(|r| match paused {
                    Some(paused) => s.pause_schedule(c, r.id, paused),
                    None => s.remove_schedule(c, r.id),
                }))
            })
            .request_schema::<ScheduleIdRequest>(true)
            .response_schema::<ScheduleDto>(200, "The schedule"),
        );
    }
    routes.extend(fade_routes(cx));
    routes
}

/// The fades on demand: `POST /volume/fade`, `POST /pause/fade`.
fn fade_routes(cx: &Ctx<'_>) -> [RouteEntry; 2] {
    [
        cx.route(
            &Op::post(
                "/volume/fade",
                "fade_volume",
                CONTROL,
                "Ramp a room, an alias or everywhere to a volume over up to 60 s",
            ),
            |s, c, req| {
                answer(body::<FadeVolumeRequest>(req).and_then(|r| s.fade_volume_to(c, &r)))
            },
        )
        .request_schema::<FadeVolumeRequest>(true)
        .response_schema::<OutcomeDto>(200, "What was done"),
        cx.route(
            &Op::post(
                "/pause/fade",
                "fade_and_pause",
                CONTROL,
                "Fade every selected group out over up to 60 s, pause it, and restore its volumes",
            ),
            |s, c, req| answer(body::<FadePauseRequest>(req).and_then(|r| s.fade_out(c, &r))),
        )
        .request_schema::<FadePauseRequest>(true)
        .response_schema::<OutcomeDto>(200, "What was done"),
    ]
}

/// `GET /sleep?zone=<room>`.
#[derive(JsonSchema)]
struct SleepQuery {
    /// A room, alias, here, all/everywhere, all@S1 or all@S2: only its groups' timers.
    zone: Option<String>,
}

fn sleep_query(req: &Request) -> Result<SleepQuery, Failure> {
    Ok(SleepQuery {
        zone: query_param(req, "zone")?,
    })
}
