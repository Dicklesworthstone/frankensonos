//! The sleep-timer and schedule tools: `set_sleep_timer`,
//! `list_sleep_timers`, `list_schedules`, `add_schedule`, `pause_schedule`,
//! `resume_schedule` and `remove_schedule` (see
//! `fsonos_api::surface::schedules`).

use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult};
use fsonos_api::surface::schedules::{
    FadePauseRequest, FadeVolumeRequest, ScheduleDto, ScheduleRequest, SleepRequest, SleepTimerDto,
};
use serde::Serialize;

use super::{Backend, from_daemon, respond, said, segment, with_backend, with_backend_off_thread};

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

/// The running timers, one line each.
fn timers_text(timers: &[SleepTimerDto]) -> String {
    if timers.is_empty() {
        return "no sleep timer runs".to_string();
    }
    timers
        .iter()
        .map(|t| t.done.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

impl Backend {
    /// The `fade_volume` tool.
    pub fn fade_volume(&self, req: &FadeVolumeRequest) -> McpResult<FinalCallToolResult> {
        respond(|| Ok(said(self.surface.fade_volume_to(&self.client, req)?)))
    }

    /// The `fade_and_pause` tool.
    pub fn fade_and_pause(&self, req: &FadePauseRequest) -> McpResult<FinalCallToolResult> {
        respond(|| Ok(said(self.surface.fade_out(&self.client, req)?)))
    }

    /// The `set_sleep_timer` tool.
    pub fn set_sleep_timer(&self, req: &SleepRequest) -> McpResult<FinalCallToolResult> {
        // The daemon's timers fade out; the speaker's own do not.
        let body = serde_json::to_value(req).ok();
        let done = |t: &SleepTimerDto| t.done.clone();
        if let Some(answer) = self.on_daemon("POST", "/sleep", body.as_ref(), done) {
            return answer;
        }
        respond(|| {
            let timer = self.surface.set_sleep_timer(&self.client, req)?;
            Ok((timer.done.clone(), timer))
        })
    }

    /// The `list_sleep_timers` tool.
    pub fn list_sleep_timers(&self, zone: Option<&str>) -> McpResult<FinalCallToolResult> {
        let path = zone.map_or_else(
            || "/sleep".to_string(),
            |z| format!("/sleep?zone={}", segment(z)),
        );
        if let Some(answer) = self
            .daemon
            .as_ref()
            .and_then(|d| d.call("GET", &path, None))
        {
            return respond(|| {
                let timers: Vec<SleepTimerDto> = from_daemon(answer, "GET", &path)?;
                Ok((timers_text(&timers), SleepTimersDto { timers }))
            });
        }
        respond(|| {
            let timers = self.surface.sleep_timers(&self.client, zone)?;
            Ok((timers_text(&timers), SleepTimersDto { timers }))
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
    description = "Ramp the volume over time: every room `zone` names (a room, one of your aliases, or `all`/`everywhere`) goes to `volume` (0-100) over `seconds` (1-60), each room kept within your caps; answers when the ramp is done, and undo_last puts the volumes back. Leave `seconds` out to let each speaker ramp itself at its own pace (about 0.8 s a step); that answers once each speaker has started (one that can't ramp itself is stepped there first). A manual change to a room meanwhile wins there. For a long fade out at bedtime, use set_sleep_timer."
)]
async fn fade_volume(
    _ctx: &McpContext,
    zone: String,
    volume: i64,
    seconds: Option<u32>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    let req = FadeVolumeRequest {
        zone,
        volume,
        fade_secs: seconds.unwrap_or(0),
    };
    with_backend_off_thread(move |b| b.fade_volume(&req)).await
}

#[tool(
    description = "Fade the group a room plays in out over `seconds` (1-60), pause it, and put its volumes back, so the next play starts at the old level; answers when done. `zone` is a room name (Room@S1 / Room@S2 picks a household; aliases work)."
)]
async fn fade_and_pause(
    _ctx: &McpContext,
    zone: String,
    seconds: u32,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    let req = FadePauseRequest {
        zone,
        fade_secs: seconds,
    };
    with_backend_off_thread(move |b| b.fade_and_pause(&req)).await
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
