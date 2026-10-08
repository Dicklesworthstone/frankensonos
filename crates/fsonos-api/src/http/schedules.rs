//! The sleep-timer and schedule routes (see [`crate::surface::schedules`]).
//!
//! | Route | Body | Answer |
//! |---|---|---|
//! | `GET /sleep?zone=<room>` | | `[SleepTimerDto]`: the group's timer, or every running one |
//! | `POST /sleep` | [`SleepRequest`] | [`SleepTimerDto`] |
//! | `GET /schedules` | | `[ScheduleDto]` |
//! | `POST /schedules` | [`ScheduleRequest`] | [`ScheduleDto`] |
//! | `POST /schedules/remove`, `/pause`, `/resume` | [`ScheduleIdRequest`] | [`ScheduleDto`] |

use fastapi::core::RouteEntry;
use fastapi::{JsonSchema, Request, fastapi_openapi};

use super::{Ctx, Op, answer, body, query_param};
use crate::failure::Failure;
use crate::surface::schedules::{
    ScheduleDto, ScheduleIdRequest, ScheduleRequest, SleepRequest, SleepTimerDto,
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
                "The sleep timers that run: a room's group's, or every group's",
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
                "Pause a room's group after a while (fading out), extend its timer, or cancel it",
            ),
            |s, c, req| answer(body::<SleepRequest>(req).and_then(|r| s.set_sleep_timer(c, &r))),
        )
        .request_schema::<SleepRequest>(true)
        .response_schema::<SleepTimerDto>(200, "The timer as it now stands"),
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
                "Start the DJ, pause, or set a volume at a time, once or every week",
            ),
            |s, c, req| answer(body::<ScheduleRequest>(req).and_then(|r| s.add_schedule(c, &r))),
        )
        .request_schema::<ScheduleRequest>(true)
        .response_schema::<ScheduleDto>(200, "The schedule added"),
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
    routes
}

/// `GET /sleep?zone=<room>`.
#[derive(JsonSchema)]
struct SleepQuery {
    /// A room: only its group's timer.
    zone: Option<String>,
}

fn sleep_query(req: &Request) -> Result<SleepQuery, Failure> {
    Ok(SleepQuery {
        zone: query_param(req, "zone")?,
    })
}
