//! The sleep-timer and schedule tools: `set_sleep_timer`,
//! `list_sleep_timers`, `list_schedules`, `add_schedule`, `pause_schedule`,
//! `resume_schedule` and `remove_schedule` (see
//! `fsonos_api::surface::schedules`).

use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult};
use fsonos_api::surface::schedules::{ScheduleDto, ScheduleRequest, SleepRequest, SleepTimerDto};
use serde::Serialize;

use super::{Backend, respond, with_backend};

/// `list_sleep_timers` structured content.
#[derive(Serialize)]
struct SleepTimersDto {
    timers: Vec<SleepTimerDto>,
}

/// `list_schedules` structured content.
#[derive(Serialize)]
struct SchedulesDto {
    schedules: Vec<ScheduleDto>,
}

impl Backend {
    /// The `set_sleep_timer` tool.
    pub fn set_sleep_timer(&self, req: &SleepRequest) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let timer = self.surface.set_sleep_timer(&self.client, req)?;
            Ok((timer.done.clone(), timer))
        })
    }

    /// The `list_sleep_timers` tool.
    pub fn list_sleep_timers(&self, zone: Option<&str>) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let timers = self.surface.sleep_timers(&self.client, zone)?;
            let text = if timers.is_empty() {
                "no sleep timer runs".to_string()
            } else {
                timers
                    .iter()
                    .map(|t| t.done.as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            Ok((text, SleepTimersDto { timers }))
        })
    }

    /// The `list_schedules` tool.
    pub fn list_schedules(&self) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let schedules = self.surface.schedules(&self.client)?;
            let text = if schedules.is_empty() {
                "no schedules".to_string()
            } else {
                schedules
                    .iter()
                    .map(ScheduleDto::line)
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            Ok((text, SchedulesDto { schedules }))
        })
    }

    /// The `add_schedule` tool.
    pub fn add_schedule(&self, req: &ScheduleRequest) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let added = self.surface.add_schedule(&self.client, req)?;
            Ok((format!("added {}", added.line()), added))
        })
    }

    /// The `pause_schedule` / `resume_schedule` tools.
    pub fn pause_schedule(&self, id: i64, paused: bool) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let schedule = self.surface.pause_schedule(&self.client, id, paused)?;
            Ok((schedule.line(), schedule))
        })
    }

    /// The `remove_schedule` tool.
    pub fn remove_schedule(&self, id: i64) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let removed = self.surface.remove_schedule(&self.client, id)?;
            Ok((format!("removed {}", removed.line()), removed))
        })
    }
}

#[tool(
    description = "Sleep timer: pause the group a room plays in after `duration` ('45m', '1h30m', '90s', or minutes as '45'; at most 23h), fading it out over the last two minutes when the daemon runs. With `extend` true, push the running timer `duration` later instead; with `cancel` true (and no duration), cancel it. `zone` is a room name (Room@S1 / Room@S2 picks a household; aliases work)."
)]
fn set_sleep_timer(
    _ctx: &McpContext,
    zone: String,
    duration: Option<String>,
    extend: Option<bool>,
    cancel: Option<bool>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    let req = SleepRequest {
        zone,
        duration,
        extend: extend.unwrap_or(false),
        cancel: cancel.unwrap_or(false),
    };
    with_backend(move |b| b.set_sleep_timer(&req))
}

#[tool(
    description = "List the sleep timers that run: when each group pauses and whether it fades out first. Give `zone` for one room's group only.",
    annotations(read_only, idempotent)
)]
fn list_sleep_timers(
    _ctx: &McpContext,
    zone: Option<String>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend(move |b| b.list_sleep_timers(zone.as_deref()))
}

#[tool(
    description = "List the schedules: what each does, when (`when`), who added it, whether it is paused, and its next run.",
    annotations(read_only, idempotent)
)]
fn list_schedules(_ctx: &McpContext) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend(Backend::list_schedules)
}

#[tool(
    description = "Schedule something, once or every week; the daemon runs it with your rights. `when`: 'in 45m', an RFC 3339 time, 'daily 22:30', 'weekdays 07:30', 'weekends 09:00', 'sat,sun 09:00', 'mon-fri 07:00' (house local time). `action`: 'dj_start' (zone), 'pause' (zone; optional fade_secs up to 600 to fade out first), 'volume' (zone and volume 0-100), or 'apply_scene' (scene). A run missed by more than 10 minutes is skipped."
)]
#[allow(clippy::too_many_arguments)] // one optional field per action
fn add_schedule(
    _ctx: &McpContext,
    when: String,
    action: String,
    zone: Option<String>,
    scene: Option<String>,
    mood: Option<String>,
    volume: Option<i64>,
    fade_secs: Option<u32>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    let req = ScheduleRequest {
        when,
        action,
        zone,
        scene,
        mood,
        volume,
        fade_secs,
    };
    with_backend(move |b| b.add_schedule(&req))
}

#[tool(
    description = "Pause a schedule by its id (from list_schedules): it does not run until resumed.",
    annotations(idempotent)
)]
fn pause_schedule(_ctx: &McpContext, id: i64) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend(move |b| b.pause_schedule(id, true))
}

#[tool(
    description = "Resume a paused schedule by its id (from list_schedules); runs missed while it was paused are not made up.",
    annotations(idempotent)
)]
fn resume_schedule(_ctx: &McpContext, id: i64) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend(move |b| b.pause_schedule(id, false))
}

#[tool(description = "Remove a schedule by its id (from list_schedules).")]
fn remove_schedule(_ctx: &McpContext, id: i64) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend(move |b| b.remove_schedule(id))
}
