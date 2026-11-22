//! The sleep-timer and schedule tools: `set_sleep_timer`,
//! `list_sleep_timers`, `list_schedules`, `add_schedule`, `pause_schedule`,
//! `resume_schedule` and `remove_schedule` (see
//! `fsonos_api::surface::schedules`).

mod update;
pub use update::UpdateSchedule;

use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult};
use fsonos_api::dj::DjDirectionDto;
use fsonos_api::surface::announce::AnnounceRequest;
use fsonos_api::surface::schedules::{
    FadePauseRequest, FadeVolumeRequest, ScheduleDto, ScheduleRequest, SleepOutcome, SleepRequest,
    SleepTimerDto,
};
use serde::Serialize;

use super::{Backend, from_daemon, respond, said, segment, with_backend_off_thread};

#[cfg(test)]
mod tests;

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
    /// The `fade_volume` tool: on the daemon when one runs, so the fade
    /// gives way to (and ends) its other fades of those rooms, else here.
    pub fn fade_volume(&self, req: &FadeVolumeRequest) -> McpResult<FinalCallToolResult> {
        self.write("/volume/fade", req, || {
            respond(|| Ok(said(self.surface.fade_volume_to(&self.client, req)?)))
        })
    }

    /// The `fade_and_pause` tool, on the daemon when one runs.
    pub fn fade_and_pause(&self, req: &FadePauseRequest) -> McpResult<FinalCallToolResult> {
        self.write("/pause/fade", req, || {
            respond(|| Ok(said(self.surface.fade_out(&self.client, req)?)))
        })
    }

    /// The `set_sleep_timer` tool.
    pub fn set_sleep_timer(&self, req: &SleepRequest) -> McpResult<FinalCallToolResult> {
        req.zone().map_err(|failure| crate::tool_error(&failure))?;
        req.change()
            .map_err(|failure| crate::tool_error(&failure))?;
        let body = serde_json::to_value(req).map_err(|error| {
            crate::tool_error(&fsonos_api::Failure::new(
                fsonos_api::ErrorCode::Internal,
                error.to_string(),
            ))
        })?;
        let mut complete = false;
        let mut result = respond(|| {
            let timers = match self
                .daemon
                .as_ref()
                .and_then(|daemon| daemon.call("POST", "/sleep", Some(&body)))
            {
                Some(answer) => from_daemon::<SleepOutcome>(answer, "POST", "/sleep")?,
                None => self.surface.set_sleep_timers(&self.client, req)?,
            };
            complete = timers.complete();
            Ok((timers.text(), timers))
        })?;
        result.is_error = !complete;
        Ok(result)
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
            let schedules = self.read("/schedules", || self.surface.schedules(&self.client))?;
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
        req.validate()
            .map_err(|failure| crate::tool_error(&failure))?;
        let body = serde_json::to_value(req).map_err(|error| {
            crate::tool_error(&fsonos_api::Failure::new(
                fsonos_api::ErrorCode::Internal,
                error.to_string(),
            ))
        })?;
        if let Some(answer) =
            self.on_daemon("POST", "/schedules", Some(&body), |added: &ScheduleDto| {
                format!("added {}", added.line())
            })
        {
            return answer;
        }
        respond(|| {
            let added = self.surface.add_schedule(&self.client, req)?;
            Ok((format!("added {}", added.line()), added))
        })
    }

    /// The `pause_schedule` / `resume_schedule` tools.
    pub fn pause_schedule(&self, id: i64, paused: bool) -> McpResult<FinalCallToolResult> {
        let path = if paused {
            "/schedules/pause"
        } else {
            "/schedules/resume"
        };
        let body = serde_json::json!({ "id": id });
        if let Some(answer) = self.on_daemon("POST", path, Some(&body), ScheduleDto::line) {
            return answer;
        }
        respond(|| {
            let schedule = self.surface.pause_schedule(&self.client, id, paused)?;
            Ok((schedule.line(), schedule))
        })
    }

    /// The `remove_schedule` tool.
    pub fn remove_schedule(&self, id: i64) -> McpResult<FinalCallToolResult> {
        let body = serde_json::json!({ "id": id });
        if let Some(answer) = self.on_daemon(
            "POST",
            "/schedules/remove",
            Some(&body),
            |removed: &ScheduleDto| format!("removed {}", removed.line()),
        ) {
            return answer;
        }
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
    ctx: &McpContext,
    zone: String,
    volume: i64,
    seconds: Option<u32>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    let req = FadeVolumeRequest {
        zone,
        volume,
        fade_secs: seconds.unwrap_or(0),
    };
    with_backend_off_thread(ctx, move |b| b.fade_volume(&req)).await
}

#[tool(
    description = "Fade every distinct group selected by `zone` out over `seconds` (1-60), pause it, and put its volumes back, so the next play starts at the old level; answers when done. `zone` accepts a room, saved alias, all/everywhere, all@S1 or all@S2. Independent groups fade concurrently. Manual changes win in their own group. Partial results retain every target and are tool errors; inspect them before another command, without replaying the batch. Optional expected_revision is the audience revision from resolve_rooms."
)]
async fn fade_and_pause(
    ctx: &McpContext,
    zone: String,
    seconds: u32,
    expected_revision: Option<String>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    let req = FadePauseRequest {
        zone,
        fade_secs: seconds,
        expected_revision,
    };
    with_backend_off_thread(ctx, move |b| b.fade_and_pause(&req)).await
}

#[tool(
    description = "Sleep timers: pause each distinct group selected by `zone` after `duration` ('45m', '1h30m', '90s', or minutes as '45'; at most 23h), fading out over the last two minutes when the daemon runs. With extend true, push each running timer duration later; a group without a timer reports its own failure. With cancel true and no duration, cancel each selected timer. zone accepts a room, saved alias, all/everywhere, all@S1 or all@S2. Room-set results retain every group's timer or failure; an incomplete result is a tool error and must not be replayed as a batch. Optional expected_revision is the audience revision from resolve_rooms."
)]
async fn set_sleep_timer(
    ctx: &McpContext,
    zone: String,
    duration: Option<String>,
    extend: Option<bool>,
    cancel: Option<bool>,
    expected_revision: Option<String>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    let req = SleepRequest {
        zone,
        duration,
        extend: extend.unwrap_or(false),
        cancel: cancel.unwrap_or(false),
        expected_revision,
    };
    with_backend_off_thread(ctx, move |b| b.set_sleep_timer(&req)).await
}

#[tool(
    description = "List the sleep timers that run: when each group pauses and whether it fades out first. Give zone to select a room, saved alias, all/everywhere, all@S1 or all@S2; omit it to list every running timer.",
    annotations(read_only, idempotent)
)]
async fn list_sleep_timers(
    ctx: &McpContext,
    zone: Option<String>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend_off_thread(ctx, move |b| b.list_sleep_timers(zone.as_deref())).await
}

#[tool(
    description = "List the schedules: what each does, when (`when`), who added it, whether it is paused, and its next run. Each current row includes an opaque revision for update_schedule; reread after an edit, pause/resume or claimed occurrence.",
    annotations(read_only, idempotent)
)]
async fn list_schedules(ctx: &McpContext) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend_off_thread(ctx, Backend::list_schedules).await
}

#[tool(
    description = "Schedule something, once or every week; the daemon runs it with your rights. `when`: 'in 45m', an RFC 3339 time, 'daily 22:30', 'weekdays 07:30', 'weekends 09:00', 'sat,sun 09:00', 'mon-fri 07:00' (house local time). `action`: 'dj_start' (zone; optional mood and direction), 'pause' (zone; optional fade_secs up to 600 to fade out first), 'volume' (zone and volume 0-100), 'apply_scene' (scene), or 'announce' (announcement). For announce, give announcement: {text: 'Time to leave', rooms: ['Kitchen', 'Office'], volume: 25, voice: 'optional voice'} or {chime: 'bell', rooms: ['Office'], volume: 25}. Give exactly one of text (at most 1000 characters) or chime (bell, beep, rise); voice is for text only, rooms defaults to all rooms and volume defaults to 35. Uploaded WAVs cannot be scheduled. Announcement fields go inside announcement, with no top-level zone, volume, mood, direction, scene or fade_secs. Each run speaks or chimes and restores the music through the usual capped announcement path. For dj_start, direction is {constraints: {include_artists: [names], exclude_artists: [names], include_genres: [genres], exclude_genres: [genres], decades: [1960], exclude_decades: [1980], include_keywords: [words], exclude_keywords: [words], include_composers: [names], exclude_composers: [names], periods: [periods], exclude_periods: [periods], min_work_minutes: 1, max_work_minutes: 60, energy_bias: -2..2, allow_long_works: false}, for_secs: 7200}; all fields are optional. Duration starts afresh at each run and requires a mood or constraint. An explicit empty direction with no mood clears prior steering to the automatic program. Omit direction for legacy mood-only behavior. A run missed by more than 10 minutes is skipped."
)]
#[allow(clippy::too_many_arguments)] // one optional field per action
async fn add_schedule(
    ctx: &McpContext,
    when: String,
    action: String,
    zone: Option<String>,
    scene: Option<String>,
    mood: Option<String>,
    direction: Option<serde_json::Value>,
    announcement: Option<serde_json::Value>,
    volume: Option<i64>,
    fade_secs: Option<u32>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    let direction = direction
        .map(serde_json::from_value::<DjDirectionDto>)
        .transpose()
        .map_err(|error| {
            crate::tool_error(&fsonos_api::Failure::invalid(format!(
                "invalid DJ direction: {error}",
            )))
        })?;
    let announcement = announcement
        .map(serde_json::from_value::<AnnounceRequest>)
        .transpose()
        .map_err(|error| {
            crate::tool_error(&fsonos_api::Failure::invalid(format!(
                "invalid scheduled announcement: {error}",
            )))
        })?;
    let req = ScheduleRequest {
        when,
        action,
        zone,
        scene,
        mood,
        direction,
        announcement,
        volume,
        fade_secs,
    };
    with_backend_off_thread(ctx, move |b| b.add_schedule(&req)).await
}

#[tool(
    description = "Pause a schedule by its id (from list_schedules): it does not run until resumed.",
    annotations(idempotent)
)]
async fn pause_schedule(
    ctx: &McpContext,
    id: i64,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend_off_thread(ctx, move |b| b.pause_schedule(id, true)).await
}

#[tool(
    description = "Resume a paused schedule by its id (from list_schedules); runs missed while it was paused are not made up.",
    annotations(idempotent)
)]
async fn resume_schedule(
    ctx: &McpContext,
    id: i64,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend_off_thread(ctx, move |b| b.pause_schedule(id, false)).await
}

#[tool(description = "Remove a schedule by its id (from list_schedules).")]
async fn remove_schedule(
    ctx: &McpContext,
    id: i64,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend_off_thread(ctx, move |b| b.remove_schedule(id)).await
}
