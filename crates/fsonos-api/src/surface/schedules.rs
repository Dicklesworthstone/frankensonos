//! Sleep timers and schedules on every surface: `fsonos sleep` and
//! `fsonos schedule`, `POST /sleep` and `/schedules`, and the MCP tools
//! `set_sleep_timer`, `add_schedule` and their companions.
//!
//! A sleep timer pauses the group a room plays in after a while. The
//! daemon's surface ([`Surface::with_sleep`]) keeps the timer and fades the
//! group out over its last minutes (`fsonos_core::sleep`), with the speaker's
//! own timer set a little later in case the daemon stops first. A surface
//! without one (the CLI on its own, `fsonos mcp`) sets only the speaker's
//! own timer, which pauses without a fade.
//!
//! Schedules live in the store, so any surface with one adds, lists, pauses
//! and removes them; `fsonos serve` runs them ([`Scheduler`], a tick a
//! second) as the client that added them, through the one control path. A
//! schedule never does more than its creator may, and each run is in the
//! action log. A run missed by more than ten minutes is skipped.

use chrono::{DateTime, FixedOffset, Local, TimeDelta, TimeZone, Utc};
use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::clock::Clock;
use fsonos_core::fade::{FadeOutcome, Fader};
use fsonos_core::policy::Client;
use fsonos_core::schedule::{
    self as core_schedule, DEFAULT_GRACE, Schedule, ScheduleAction, ScheduleError, ScheduleSpec,
    Tick,
};
use fsonos_core::sleep::{FADE, SleepOutcome, SleepTimers};
use fsonos_core::{CoreError, HouseholdState};
use fsonos_proto::{ProtoError, control as soap};
use fsonos_types::PlayerId;
use serde::{Deserialize, Serialize};
use std::fmt::{self, Write as _};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::{Surface, room_view};
use crate::failure::{ErrorCode, Failure};
use crate::plan::{self, TransportAction, resolve};
use crate::request::{DjStartRequest, VolumeRequest, ZoneRequest, zone_name};

/// The longest sleep timer. The speaker's own timer, which backs the
/// daemon's up five minutes later, counts down from just under a day.
pub const MAX_SLEEP: Duration = Duration::from_hours(23);

/// The longest fade a scheduled pause takes, in seconds.
pub const MAX_PAUSE_FADE_SECS: u32 = 600;

/// How often [`Scheduler::start`] ticks.
pub const TICK: Duration = Duration::from_secs(1);

/// How long a stopping scheduler waits for the runs it started.
pub const SHUTDOWN_WAIT: Duration = Duration::from_secs(5);

/// A duration as people type it: `45m`, `1h30m`, `1h 30m`, `90s`, `2h`, or
/// a bare number of minutes (`45`).
#[must_use]
pub fn parse_duration(input: &str) -> Option<Duration> {
    let s: String = input
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_lowercase();
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
        let minutes: u64 = s.parse().ok()?;
        return (minutes > 0).then(|| Duration::from_secs(minutes.saturating_mul(60)));
    }
    let (mut total, mut number) = (0_u64, String::new());
    for c in s.chars() {
        if c.is_ascii_digit() {
            number.push(c);
            continue;
        }
        let n: u64 = number.parse().ok()?;
        number.clear();
        let unit = match c {
            'h' => 3600,
            'm' => 60,
            's' => 1,
            _ => return None,
        };
        total = total.checked_add(n.checked_mul(unit)?)?;
    }
    (number.is_empty() && total > 0).then(|| Duration::from_secs(total))
}

/// `d` the way [`parse_duration`] reads it: `1h30m`, `45m`, `1m30s`.
#[must_use]
pub fn span(d: Duration) -> String {
    let secs = d.as_secs();
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    let mut out = String::new();
    if h > 0 {
        let _ = write!(out, "{h}h");
    }
    if m > 0 {
        let _ = write!(out, "{m}m");
    }
    if s > 0 || out.is_empty() {
        let _ = write!(out, "{s}s");
    }
    out
}

/// `POST /sleep` body (the `set_sleep_timer` tool): pause the group a room
/// plays in after `duration`, push its timer `duration` later (`extend`),
/// or cancel it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SleepRequest {
    pub zone: String,
    /// How long until it pauses: `45m`, `1h30m`, `90s`, or minutes (`45`);
    /// at most 23 hours.
    #[serde(default)]
    pub duration: Option<String>,
    /// Push the group's timer `duration` later instead.
    #[serde(default)]
    pub extend: bool,
    /// Cancel the group's timer (with no `duration`).
    #[serde(default)]
    pub cancel: bool,
}

/// What a [`SleepRequest`] asks for, checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SleepChange {
    /// Pause after this long, replacing any timer.
    Set(Duration),
    /// Push the timer this much later.
    Extend(Duration),
    Cancel,
}

impl fmt::Display for SleepChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Set(after) => write!(f, "in {}", span(*after)),
            Self::Extend(by) => write!(f, "{} later", span(*by)),
            Self::Cancel => f.write_str("cancel"),
        }
    }
}

impl SleepRequest {
    /// The trimmed zone name, or why it is unusable.
    pub fn zone(&self) -> Result<&str, Failure> {
        zone_name("zone", &self.zone)
    }

    /// The change asked for, or why it is unusable.
    pub fn change(&self) -> Result<SleepChange, Failure> {
        let duration = || -> Result<Duration, Failure> {
            let raw = self.duration.as_deref().unwrap_or_default().trim();
            let d = parse_duration(raw).ok_or_else(|| {
                Failure::invalid(format!("duration {raw:?} is not a duration"))
                    .with_hint("Write it like 45m, 1h30m or 90s (a bare number is minutes).")
            })?;
            if d > MAX_SLEEP {
                return Err(too_long(d));
            }
            Ok(d)
        };
        match (self.cancel, self.extend, self.duration.is_some()) {
            (true, false, false) => Ok(SleepChange::Cancel),
            (true, _, _) => Err(Failure::invalid(
                "`cancel` takes no `duration` and no `extend`",
            )),
            (false, _, false) => Err(Failure::invalid(
                "give a `duration` (45m, 1h30m), or `cancel` true",
            )),
            (false, true, true) => duration().map(SleepChange::Extend),
            (false, false, true) => duration().map(SleepChange::Set),
        }
    }
}

fn too_long(d: Duration) -> Failure {
    Failure::invalid(format!(
        "a sleep timer of {} is longer than {}",
        span(d),
        span(MAX_SLEEP)
    ))
    .with_hint("Use a schedule (add_schedule) to pause at a time further off.")
}

/// A sleep timer (`set_sleep_timer`, `list_sleep_timers`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SleepTimerDto {
    /// The group, named by its coordinator's room.
    pub zone: String,
    /// When the group pauses (RFC 3339); absent when no timer runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ends: Option<String>,
    /// Seconds until then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_secs: Option<u64>,
    /// The daemon fades the group out over the timer's last minutes; false:
    /// the speaker's own timer pauses it, without a fade.
    pub fades: bool,
    /// The fade has begun.
    pub fading: bool,
    /// What was done, or how the timer stands, in a sentence.
    pub done: String,
    /// Whether anything was sent to a speaker.
    pub changed: bool,
}

/// `POST /schedules` body (the `add_schedule` tool): do `action` at `when`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScheduleRequest {
    /// When it runs: `in 45m`, an RFC 3339 time, `daily 22:30`,
    /// `weekdays 07:30`, `weekends 09:00`, `sat,sun 09:00`, `mon-fri 07:00`.
    /// Wall times are the house's local time.
    pub when: String,
    /// What it does: `dj_start` (zone), `pause` (zone, optional fade_secs),
    /// `volume` (zone, volume) or `apply_scene` (scene).
    pub action: String,
    /// The room it acts on (every action but apply_scene).
    #[serde(default)]
    pub zone: Option<String>,
    /// apply_scene: the scene's name.
    #[serde(default)]
    pub scene: Option<String>,
    /// dj_start: a DJ mood to start in.
    #[serde(default)]
    pub mood: Option<String>,
    /// volume: the level, 0-100.
    #[serde(default)]
    pub volume: Option<i64>,
    /// pause: fade out over this many seconds first (at most 600).
    #[serde(default)]
    pub fade_secs: Option<u32>,
}

impl ScheduleRequest {
    /// The action asked for, checked (its rooms are resolved when it is
    /// added).
    pub fn action(&self) -> Result<ScheduleAction, Failure> {
        let kind = self.action.trim().to_lowercase().replace('-', "_");
        let zone = || -> Result<String, Failure> {
            let raw = self
                .zone
                .as_deref()
                .ok_or_else(|| Failure::invalid(format!("a {kind} schedule needs a `zone`")))?;
            Ok(zone_name("zone", raw)?.to_string())
        };
        let refuse = |fields: &[(&str, bool)]| -> Result<(), Failure> {
            match fields.iter().find(|(_, set)| *set) {
                Some((field, _)) => Err(Failure::invalid(format!(
                    "a {kind} schedule takes no `{field}`"
                ))),
                None => Ok(()),
            }
        };
        let (zone_set, scene_set, mood_set) = (
            self.zone.is_some(),
            self.scene.is_some(),
            self.mood.is_some(),
        );
        let (volume_set, fade_set) = (self.volume.is_some(), self.fade_secs.is_some());
        match kind.as_str() {
            "dj_start" => {
                refuse(&[
                    ("scene", scene_set),
                    ("volume", volume_set),
                    ("fade_secs", fade_set),
                ])?;
                Ok(ScheduleAction::DjStart {
                    room: zone()?,
                    mood: self
                        .mood
                        .as_deref()
                        .map(str::trim)
                        .filter(|m| !m.is_empty())
                        .map(str::to_lowercase),
                })
            }
            "pause" => {
                refuse(&[
                    ("scene", scene_set),
                    ("mood", mood_set),
                    ("volume", volume_set),
                ])?;
                let fade_secs = self.fade_secs.unwrap_or(0);
                if fade_secs > MAX_PAUSE_FADE_SECS {
                    return Err(Failure::invalid(format!(
                        "fade_secs {fade_secs} is more than {MAX_PAUSE_FADE_SECS}"
                    )));
                }
                Ok(ScheduleAction::Pause {
                    room: zone()?,
                    fade_secs,
                })
            }
            "volume" => {
                refuse(&[
                    ("scene", scene_set),
                    ("mood", mood_set),
                    ("fade_secs", fade_set),
                ])?;
                let raw = self.volume.ok_or_else(|| {
                    Failure::invalid("a volume schedule needs a `volume` (0 to 100)")
                })?;
                let level = u8::try_from(raw)
                    .ok()
                    .filter(|v| *v <= 100)
                    .ok_or_else(|| {
                        Failure::invalid(format!("volume must be 0 to 100, got {raw}"))
                    })?;
                Ok(ScheduleAction::Volume {
                    room: zone()?,
                    level,
                })
            }
            "apply_scene" | "scene" => {
                refuse(&[
                    ("zone", zone_set),
                    ("mood", mood_set),
                    ("volume", volume_set),
                    ("fade_secs", fade_set),
                ])?;
                let name = self
                    .scene
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| Failure::invalid("an apply_scene schedule needs a `scene`"))?;
                Ok(ScheduleAction::ApplyScene { name: name.into() })
            }
            _ => Err(
                Failure::invalid(format!("{:?} is not a schedule action", self.action))
                    .with_hint("Use dj_start, pause, volume or apply_scene."),
            ),
        }
    }
}

/// `POST /schedules/remove|pause|resume` body: the schedule's id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScheduleIdRequest {
    /// As `list_schedules` (GET /schedules) shows it.
    pub id: i64,
}

/// A stored schedule (`list_schedules`, `add_schedule`, ...).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ScheduleDto {
    pub id: i64,
    /// When it runs, as stored: `weekdays 07:30`, or `once <RFC 3339>`.
    pub when: String,
    /// `dj_start`, `pause`, `volume` or `apply_scene`.
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zone: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scene: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mood: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fade_secs: Option<u32>,
    /// What it does, in words.
    pub summary: String,
    /// The client that added it: each run has that client's rights.
    pub creator: String,
    /// False while paused.
    pub enabled: bool,
    /// Its next run (RFC 3339, local time); absent when paused or done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    /// Its last run, or the last run skipped as too late.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fired: Option<String>,
}

impl ScheduleDto {
    fn new(s: &Schedule, now: DateTime<Utc>) -> Self {
        let local = |t: DateTime<Utc>| t.with_timezone(&Local).to_rfc3339();
        let (mut zone, mut scene, mut mood, mut volume, mut fade_secs) =
            (None, None, None, None, None);
        match &s.action {
            ScheduleAction::DjStart { room, mood: m } => {
                zone = Some(room.clone());
                mood.clone_from(m);
            }
            ScheduleAction::Pause { room, fade_secs: f } => {
                zone = Some(room.clone());
                fade_secs = (*f > 0).then_some(*f);
            }
            ScheduleAction::Volume { room, level } => {
                zone = Some(room.clone());
                volume = Some(*level);
            }
            ScheduleAction::ApplyScene { name } => scene = Some(name.clone()),
        }
        Self {
            id: s.id,
            when: s.spec.to_string(),
            action: action_kind(&s.action).into(),
            zone,
            scene,
            mood,
            volume,
            fade_secs,
            summary: summary(&s.action),
            creator: s.creator.clone(),
            enabled: s.enabled,
            next: s.enabled.then(|| s.next(now, &Local)).flatten().map(local),
            last_fired: s.last_fired.map(local),
        }
    }

    /// One line: `#3 weekdays 07:30: start the DJ in Kitchen (next ...)`.
    #[must_use]
    pub fn line(&self) -> String {
        let state = match (&self.next, self.enabled) {
            (_, false) => " (paused)".to_string(),
            (Some(next), true) => format!(" (next {next})"),
            (None, true) => " (done)".to_string(),
        };
        format!("#{} {}: {}{state}", self.id, self.when, self.summary)
    }
}

fn action_kind(action: &ScheduleAction) -> &'static str {
    match action {
        ScheduleAction::DjStart { .. } => "dj_start",
        ScheduleAction::Pause { .. } => "pause",
        ScheduleAction::Volume { .. } => "volume",
        ScheduleAction::ApplyScene { .. } => "apply_scene",
    }
}

/// The tool a run of `action` is, under the house policy.
fn action_tool(action: &ScheduleAction) -> &'static str {
    match action {
        ScheduleAction::Volume { .. } => "set_volume",
        other => action_kind(other),
    }
}

/// What `action` does, in words.
#[must_use]
pub fn summary(action: &ScheduleAction) -> String {
    match action {
        ScheduleAction::DjStart { room, mood: None } => format!("start the DJ in {room}"),
        ScheduleAction::DjStart {
            room,
            mood: Some(mood),
        } => format!("start the DJ in {room} (mood {mood})"),
        ScheduleAction::Pause { room, fade_secs: 0 } => format!("pause {room}"),
        ScheduleAction::Pause { room, fade_secs } => format!(
            "pause {room} after a {} fade",
            span(Duration::from_secs(u64::from(*fade_secs)))
        ),
        ScheduleAction::Volume { room, level } => format!("set {room}'s volume to {level}"),
        ScheduleAction::ApplyScene { name } => format!("apply the scene {name}"),
    }
}

/// The client a policy key names: the inverse of [`Client::key`].
#[must_use]
pub fn client_of(key: &str) -> Client {
    match key {
        "cli" => Client::Cli,
        "mcp-stdio" => Client::McpStdio,
        "loopback-http" => Client::LoopbackHttp,
        "unknown" => Client::Unknown,
        principal => Client::Tailnet(principal.to_string()),
    }
}

/// The daemon's sleep timers and the fader that fades them out. Give one to
/// the daemon's surface ([`Surface::with_sleep`]); its [`Scheduler`] runs
/// them.
pub struct Sleep {
    timers: SleepTimers,
    fader: Fader,
}

impl Default for Sleep {
    fn default() -> Self {
        Self::with_fade(FADE)
    }
}

impl Sleep {
    /// Timers that fade out over `fade` (two minutes by default).
    #[must_use]
    pub fn with_fade(fade: Duration) -> Self {
        Self {
            timers: SleepTimers::with_fade(fade),
            fader: Fader::default(),
        }
    }
}

/// A group, as a sleep timer addresses it.
struct Zone {
    coordinator: PlayerId,
    ip: IpAddr,
    /// The coordinator's room, which names the group.
    room: String,
}

impl Zone {
    fn of(households: &[HouseholdState], coordinator: &PlayerId) -> Option<Self> {
        let household = households
            .iter()
            .find(|h| h.player(coordinator).is_some())?;
        let player = household.player(coordinator)?;
        let room = household
            .rooms
            .iter()
            .find(|r| r.players.contains(coordinator))
            .map_or_else(|| player.room_name.clone(), |r| r.name.clone());
        Some(Self {
            coordinator: coordinator.clone(),
            ip: player.ip,
            room,
        })
    }
}

fn proto(e: ProtoError) -> Failure {
    Failure::from(CoreError::from(e))
}

fn delta(d: Duration) -> TimeDelta {
    TimeDelta::from_std(d).unwrap_or(TimeDelta::MAX)
}

fn secs(d: Duration) -> u32 {
    u32::try_from(d.as_secs()).unwrap_or(u32::MAX)
}

/// The speaker's own timer: how long it has left.
fn speaker_timer(
    t: &dyn fsonos_proto::Transport,
    zone: &Zone,
) -> Result<Option<Duration>, Failure> {
    Ok(soap::get_remaining_sleep_timer(t, zone.ip)
        .map_err(proto)?
        .map(|s| Duration::from_secs(u64::from(s))))
}

/// The answer to a cancel: whether there was a timer to cancel.
fn cancelled(zone: &Zone, had: bool) -> SleepTimerDto {
    SleepTimerDto {
        zone: zone.room.clone(),
        ends: None,
        remaining_secs: None,
        fades: false,
        fading: false,
        done: if had {
            format!("cancelled the sleep timer of {}'s group", zone.room)
        } else {
            format!("{}'s group had no sleep timer", zone.room)
        },
        changed: had,
    }
}

fn no_timer(zone: &Zone) -> Failure {
    Failure::invalid(format!(
        "{}'s group has no sleep timer to extend",
        zone.room
    ))
    .with_hint("Set one first: give a duration without extend.")
}

fn no_store() -> Failure {
    Failure::new(
        ErrorCode::NotImplemented,
        "schedules are kept in the store, which this surface does not have",
    )
    .with_hint("Give fsonos a data directory (FSONOS_DATA_DIR); fsonos serve runs the schedules.")
}

fn schedule_failure(e: &ScheduleError) -> Failure {
    match e {
        ScheduleError::Unrecognized(_) | ScheduleError::Past(_) => Failure::invalid(e.to_string())
            .with_hint(
                "Write when like \"in 45m\", \"daily 22:30\", \"weekdays 07:30\", \
                 \"sat,sun 09:00\" or an RFC 3339 time.",
            ),
        ScheduleError::Corrupt(_) | ScheduleError::Store(_) => {
            Failure::new(ErrorCode::Internal, e.to_string())
        }
    }
}

/// The `dj_start` request a scheduled DJ start makes.
fn dj_start(room: &str, mood: Option<&str>) -> DjStartRequest {
    DjStartRequest {
        zone: room.to_string(),
        mood: mood.map(str::to_string),
        for_secs: None,
    }
}

impl Surface {
    /// Tell the time by `clock`: when schedules and sleep timers are due,
    /// and every other time the surface reads (`fsonos serve --clock-file`
    /// lets a test move it).
    #[must_use]
    pub fn with_clock(mut self, clock: Box<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Keep sleep timers here (the daemon's surface), so they fade out; a
    /// [`Scheduler`] over this surface runs them.
    #[must_use]
    pub fn with_sleep(mut self, sleep: Sleep) -> Self {
        self.sleep = Some(Arc::new(sleep));
        self
    }

    /// Authorize the write `tool`; a denial is logged, as [`Self::control`]
    /// logs one.
    pub(super) fn authorize_write(&self, client: &Client, tool: &str) -> Result<(), Failure> {
        self.guard(client)
            .authorize(tool, false)
            .inspect_err(|denied| {
                self.record(
                    client,
                    tool.to_string(),
                    format!("deny: {}", denied.detail),
                    denied.detail.clone(),
                    None,
                );
            })
    }

    /// Log a write that went through (or failed): there is no before-state
    /// to undo.
    pub(super) fn log_write<T>(
        &self,
        client: &Client,
        intent: String,
        result: &Result<T, Failure>,
        done: impl FnOnce(&T) -> String,
    ) {
        let text = match result {
            Ok(value) => done(value),
            Err(f) => format!("failed: {}", f.detail),
        };
        self.record(client, intent, "allow".into(), text, None);
    }

    /// The group `zone` plays in.
    fn zone_of(&self, client: &Client, zone: &str) -> Result<(Vec<HouseholdState>, Zone), Failure> {
        let households = self.households()?;
        let aliases = self.aliases();
        let coordinator = resolve(room_view(&households, aliases.as_ref(), client), zone)
            .map_err(|f| self.explain(f))?
            .coordinator
            .id
            .clone();
        let zone = Zone::of(&households, &coordinator)
            .ok_or_else(|| Failure::new(ErrorCode::Internal, "the group's coordinator is gone"))?;
        Ok((households, zone))
    }

    /// Set, extend or cancel the sleep timer of the group `req.zone` plays
    /// in (`set_sleep_timer`).
    pub fn set_sleep_timer(
        &self,
        client: &Client,
        req: &SleepRequest,
    ) -> Result<SleepTimerDto, Failure> {
        self.authorize_write(client, "set_sleep_timer")?;
        let change = req.change()?;
        let (households, zone) = self.zone_of(client, req.zone()?)?;
        let now = self.clock.now();
        let result = match &self.sleep {
            Some(sleep) => self.sleep_in_daemon(sleep, &households, &zone, change, now),
            None => self.sleep_on_speaker(&zone, change, now),
        };
        if let Err(f) = &result {
            self.notice(f);
        }
        let intent = format!("set_sleep_timer: {}'s group {change}", zone.room);
        self.log_write(client, intent, &result, |d| d.done.clone());
        result
    }

    fn sleep_in_daemon(
        &self,
        sleep: &Sleep,
        households: &[HouseholdState],
        zone: &Zone,
        change: SleepChange,
        now: DateTime<FixedOffset>,
    ) -> Result<SleepTimerDto, Failure> {
        let (t, fader, utc) = (&*self.transport, &sleep.fader, now.to_utc());
        let at = &zone.coordinator;
        let timer = match change {
            SleepChange::Set(after) => sleep.timers.start(t, households, fader, at, after, utc)?,
            SleepChange::Extend(by) => {
                if let Some(timer) = sleep.timers.get(at) {
                    let after = (timer.ends.max(utc) - utc).to_std().unwrap_or_default() + by;
                    if after > MAX_SLEEP {
                        return Err(too_long(after));
                    }
                    sleep
                        .timers
                        .extend(t, households, fader, at, by, utc)?
                        .ok_or_else(|| no_timer(zone))?
                } else {
                    // The speaker's own timer (the Sonos app's, say): the
                    // daemon takes it over, and fades it out.
                    let left = speaker_timer(t, zone)?.ok_or_else(|| no_timer(zone))?;
                    if left + by > MAX_SLEEP {
                        return Err(too_long(left + by));
                    }
                    sleep
                        .timers
                        .start(t, households, fader, at, left + by, utc)?
                }
            }
            SleepChange::Cancel => {
                let mut had = sleep.timers.cancel(t, households, fader, at)?;
                // A timer of the speaker's own goes too.
                if !had && speaker_timer(t, zone)?.is_some() {
                    soap::configure_sleep_timer(t, zone.ip, None).map_err(proto)?;
                    had = true;
                }
                return Ok(cancelled(zone, had));
            }
        };
        let left = (timer.ends - utc).to_std().unwrap_or_default();
        let ends = timer.ends.with_timezone(now.offset());
        let verb = if matches!(change, SleepChange::Extend(_)) {
            "now pauses"
        } else {
            "pauses"
        };
        Ok(SleepTimerDto {
            zone: zone.room.clone(),
            ends: Some(ends.to_rfc3339()),
            remaining_secs: Some(left.as_secs()),
            fades: true,
            fading: false,
            done: format!(
                "{}'s group {verb} in {} (at {}), fading out first",
                zone.room,
                span(left),
                ends.format("%H:%M")
            ),
            changed: true,
        })
    }

    fn sleep_on_speaker(
        &self,
        zone: &Zone,
        change: SleepChange,
        now: DateTime<FixedOffset>,
    ) -> Result<SleepTimerDto, Failure> {
        let t = &*self.transport;
        let after = match change {
            SleepChange::Set(after) => after,
            SleepChange::Extend(by) => {
                let left = speaker_timer(t, zone)?.ok_or_else(|| no_timer(zone))?;
                if left + by > MAX_SLEEP {
                    return Err(too_long(left + by));
                }
                left + by
            }
            SleepChange::Cancel => {
                let had = speaker_timer(t, zone)?.is_some();
                if had {
                    soap::configure_sleep_timer(t, zone.ip, None).map_err(proto)?;
                }
                return Ok(cancelled(zone, had));
            }
        };
        soap::configure_sleep_timer(t, zone.ip, Some(secs(after))).map_err(proto)?;
        let ends = now + delta(after);
        Ok(SleepTimerDto {
            zone: zone.room.clone(),
            ends: Some(ends.to_rfc3339()),
            remaining_secs: Some(after.as_secs()),
            fades: false,
            fading: false,
            done: format!(
                "{}'s group pauses in {} (at {}) by the speaker's own timer, without a fade \
                 (fsonos serve fades it out)",
                zone.room,
                span(after),
                ends.format("%H:%M")
            ),
            changed: true,
        })
    }

    /// The sleep timers that run (`list_sleep_timers`, read-only): of the
    /// group `zone` plays in, or of every group.
    pub fn sleep_timers(
        &self,
        client: &Client,
        zone: Option<&str>,
    ) -> Result<Vec<SleepTimerDto>, Failure> {
        self.guard(client).authorize("list_sleep_timers", true)?;
        let zones = if let Some(z) = zone {
            vec![self.zone_of(client, z)?.1]
        } else {
            let households = self.households()?;
            households
                .iter()
                .flat_map(|h| &h.groups)
                .filter_map(|g| Zone::of(&households, &g.coordinator))
                .collect()
        };
        let now = self.clock.now();
        let mut timers = Vec::new();
        for z in zones {
            if let Some(timer) = self
                .sleep
                .as_ref()
                .and_then(|s| s.timers.get(&z.coordinator))
            {
                let left = (timer.ends - now.to_utc()).to_std().unwrap_or_default();
                let state = if timer.fading {
                    "is fading out"
                } else {
                    "pauses"
                };
                timers.push(SleepTimerDto {
                    done: format!("{}'s group {state} in {}", z.room, span(left)),
                    zone: z.room,
                    ends: Some(timer.ends.with_timezone(now.offset()).to_rfc3339()),
                    remaining_secs: Some(left.as_secs()),
                    fades: true,
                    fading: timer.fading,
                    changed: false,
                });
                continue;
            }
            match speaker_timer(&*self.transport, &z) {
                Ok(Some(left)) => timers.push(SleepTimerDto {
                    done: format!(
                        "{}'s group pauses in {} by the speaker's own timer",
                        z.room,
                        span(left)
                    ),
                    zone: z.room,
                    ends: Some((now + delta(left)).to_rfc3339()),
                    remaining_secs: Some(left.as_secs()),
                    fades: false,
                    fading: false,
                    changed: false,
                }),
                Ok(None) => {}
                Err(f) if zone.is_some() => return Err(f),
                Err(f) => tracing::debug!("no sleep timer read from {}: {}", z.room, f.detail),
            }
        }
        Ok(timers)
    }

    /// Every stored schedule (`list_schedules`, read-only).
    pub fn schedules(&self, client: &Client) -> Result<Vec<ScheduleDto>, Failure> {
        self.guard(client).authorize("list_schedules", true)?;
        let now = self.clock.now().to_utc();
        Ok(self
            .load_schedules()?
            .iter()
            .map(|s| ScheduleDto::new(s, now))
            .collect())
    }

    /// The stored schedules; an unreadable one is logged and left out.
    fn load_schedules(&self) -> Result<Vec<Schedule>, Failure> {
        let stored = self.with_store(|s| s.schedules())?.ok_or_else(no_store)?;
        Ok(stored
            .iter()
            .filter_map(|s| {
                Schedule::from_stored(s)
                    .inspect_err(|e| tracing::warn!("schedule #{} ignored: {e}", s.id))
                    .ok()
            })
            .collect())
    }

    fn find_schedule(&self, id: i64) -> Result<Schedule, Failure> {
        let schedules = self.load_schedules()?;
        let ids: Vec<String> = schedules.iter().map(|s| s.id.to_string()).collect();
        schedules.into_iter().find(|s| s.id == id).ok_or_else(|| {
            Failure::new(ErrorCode::UnknownSchedule, format!("no schedule #{id}"))
                .with_suggestions(ids)
        })
    }

    /// Add a schedule run with `client`'s rights (`add_schedule`). Its rooms
    /// must exist now, and `client` must be allowed what it does.
    pub fn add_schedule(
        &self,
        client: &Client,
        req: &ScheduleRequest,
    ) -> Result<ScheduleDto, Failure> {
        self.authorize_write(client, "add_schedule")?;
        let action = req.action()?;
        self.authorize_write(client, action_tool(&action))?;
        let now = self.clock.now();
        let spec = ScheduleSpec::parse(&req.when, now).map_err(|e| schedule_failure(&e))?;
        match &action {
            // A saved scene (UNKNOWN_SCENE with the nearest names if not).
            ScheduleAction::ApplyScene { name } => {
                self.scene(client, name)?;
            }
            ScheduleAction::DjStart { room, mood } => {
                self.zone_of(client, room)?;
                if let Some(mood) = mood {
                    self.check_mood(client, room, mood)?;
                }
            }
            ScheduleAction::Pause { room, .. } | ScheduleAction::Volume { room, .. } => {
                self.zone_of(client, room)?;
            }
        }
        let created = now.to_utc();
        let result = self
            .with_store(|s| Ok(core_schedule::add(s, &spec, &action, client.key(), created)))?
            .ok_or_else(no_store)?
            .map_err(|e| schedule_failure(&e))
            .map(|id| Schedule {
                id,
                spec,
                action: action.clone(),
                creator: client.key().to_string(),
                enabled: true,
                created: DateTime::from_timestamp(created.timestamp(), 0).unwrap_or(created),
                last_fired: None,
            })
            .map(|s| ScheduleDto::new(&s, created));
        let intent = format!("add_schedule: {spec}: {}", summary(&action));
        self.log_write(client, intent, &result, |d| format!("added {}", d.line()));
        result
    }

    /// `mood` is one of the DJ's moods (built in, or in moods.toml).
    fn check_mood(&self, client: &Client, room: &str, mood: &str) -> Result<(), Failure> {
        dj_start(room, Some(mood)).steer()?;
        let moods = self.dj_moods(client, None)?.moods;
        if moods.iter().any(|m| m.name.eq_ignore_ascii_case(mood)) {
            return Ok(());
        }
        Err(Failure::new(
            ErrorCode::UnknownMood,
            format!("no DJ mood is called {mood:?}"),
        )
        .with_suggestions(moods.into_iter().map(|m| m.name)))
    }

    /// Remove schedule `id` (`remove_schedule`); answers what it was.
    pub fn remove_schedule(&self, client: &Client, id: i64) -> Result<ScheduleDto, Failure> {
        self.authorize_write(client, "remove_schedule")?;
        let schedule = self.find_schedule(id)?;
        let dto = ScheduleDto::new(&schedule, self.clock.now().to_utc());
        let result = self
            .with_store(|s| s.delete_schedule(id))?
            .ok_or_else(no_store)
            .map(|_| dto);
        let intent = format!("remove_schedule: #{id}");
        self.log_write(client, intent, &result, |d| format!("removed {}", d.line()));
        result
    }

    /// Pause schedule `id` (`pause_schedule`), or resume it
    /// (`resume_schedule`). A resumed schedule runs next at its next time
    /// from now: the runs it missed while paused are not made up.
    pub fn pause_schedule(
        &self,
        client: &Client,
        id: i64,
        paused: bool,
    ) -> Result<ScheduleDto, Failure> {
        let tool = if paused {
            "pause_schedule"
        } else {
            "resume_schedule"
        };
        self.authorize_write(client, tool)?;
        let mut schedule = self.find_schedule(id)?;
        let now = self.clock.now().to_utc();
        let result = self
            .with_store(|s| {
                if !paused && schedule.enabled {
                    return Ok(true);
                }
                // Resuming skips the runs missed while paused.
                if !paused {
                    s.mark_schedule_fired(id, now.timestamp())?;
                }
                s.set_schedule_enabled(id, !paused)
            })?
            .ok_or_else(no_store)
            .map(|_| {
                schedule.enabled = !paused;
                if !paused {
                    schedule.last_fired = schedule.last_fired.max(Some(now));
                }
                ScheduleDto::new(&schedule, now)
            });
        let intent = format!("{tool}: #{id}");
        self.log_write(client, intent, &result, ScheduleDto::line);
        result
    }

    /// Fade the group `room` plays in out over `over` and pause it, putting
    /// the volumes back (a scheduled pause with a fade).
    fn fade_and_pause(
        &self,
        client: &Client,
        room: &str,
        over: Duration,
    ) -> Result<String, Failure> {
        self.authorize_write(client, "pause")?;
        let (households, zone) = self.zone_of(client, room)?;
        let own;
        let fader = if let Some(sleep) = &self.sleep {
            &sleep.fader
        } else {
            own = Fader::default();
            &own
        };
        let result = fader
            .fade_out_and_pause(&*self.transport, &households, &zone.coordinator, over)
            .map_err(Failure::from)
            .map(|outcome| match outcome {
                FadeOutcome::Reached => {
                    format!(
                        "faded {}'s group out over {} and paused it",
                        zone.room,
                        span(over)
                    )
                }
                FadeOutcome::Superseded { .. } => format!(
                    "stopped fading {}'s group: its volume was changed meanwhile",
                    zone.room
                ),
            });
        let intent = format!("pause: {}'s group after a {} fade", zone.room, span(over));
        self.log_write(client, intent, &result, Clone::clone);
        result
    }
}

/// Carry `schedule` out as its creator, through the surface's one control
/// path; answers what was done.
fn fire(surface: &Surface, schedule: &Schedule) -> Result<String, Failure> {
    let client = client_of(&schedule.creator);
    let zone = |room: &str| ZoneRequest { zone: room.into() };
    match &schedule.action {
        ScheduleAction::DjStart { room, mood } => {
            let req = dj_start(room, mood.as_deref());
            surface
                .control(&client, "dj_start", |rooms| {
                    plan::plan_dj_start(rooms, &req)
                })
                .map(|o| o.done)
        }
        ScheduleAction::Pause { room, fade_secs: 0 } => surface
            .control(&client, "pause", |rooms| {
                plan::plan_transport(rooms, &zone(room), TransportAction::Pause)
            })
            .map(|o| o.done),
        ScheduleAction::Pause { room, fade_secs } => {
            surface.fade_and_pause(&client, room, Duration::from_secs(u64::from(*fade_secs)))
        }
        ScheduleAction::Volume { room, level } => {
            let req = VolumeRequest {
                zone: room.clone(),
                volume: Some(i64::from(*level)),
                delta: None,
                group: false,
            };
            surface
                .control(&client, "set_volume", |rooms| {
                    plan::plan_volume(rooms, &req)
                })
                .map(|o| o.done)
        }
        ScheduleAction::ApplyScene { name } => surface.apply_scene(&client, name).map(|applied| {
            if applied.complete {
                return applied.done;
            }
            let failed: Vec<String> = applied
                .failed
                .iter()
                .map(|f| format!("{}: {}", f.step, f.error))
                .collect();
            format!("{} (failed: {})", applied.done, failed.join("; "))
        }),
    }
}

/// What a scheduler tick started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ran {
    /// A sleep timer's fade began, in the group named by `zone`.
    Fading { zone: String },
    /// Schedule `id`'s run due at `at` began.
    Fired { id: i64, at: DateTime<Utc> },
    /// Schedule `id`'s run due at `at` was too late to start: recorded as
    /// done, not run.
    Skipped { id: i64, at: DateTime<Utc> },
}

/// Runs a daemon surface's sleep timers and stored schedules: each tick
/// starts the fades and the schedule runs that are due, each on a thread of
/// its own, so a slow speaker never holds up the next tick.
pub struct Scheduler {
    surface: Arc<Surface>,
    grace: Duration,
    runs: Mutex<Vec<JoinHandle<()>>>,
}

impl Scheduler {
    /// A scheduler over `surface`; it does nothing until ticked.
    #[must_use]
    pub fn new(surface: &Arc<Surface>) -> Self {
        Self {
            surface: Arc::clone(surface),
            grace: DEFAULT_GRACE,
            runs: Mutex::new(Vec::new()),
        }
    }

    /// One tick at the surface clock's now, in the host's time zone.
    pub fn tick(&self) -> Vec<Ran> {
        self.tick_at(self.surface.clock.now().to_utc(), &Local)
    }

    /// One tick at `now`, with wall times in `tz`.
    pub fn tick_at<Tz: TimeZone>(&self, now: DateTime<Utc>, tz: &Tz) -> Vec<Ran> {
        let mut ran = self.start_fades(now);
        ran.extend(self.run_schedules(now, tz));
        ran
    }

    fn start_fades(&self, now: DateTime<Utc>) -> Vec<Ran> {
        let Some(sleep) = self.surface.sleep.clone() else {
            return Vec::new();
        };
        sleep
            .timers
            .due(now)
            .into_iter()
            .map(|timer| {
                let (surface, sleep) = (Arc::clone(&self.surface), Arc::clone(&sleep));
                let zone = timer.room.clone();
                self.spawn("fsonos-sleep", move || {
                    let households = surface.households().unwrap_or_default();
                    let ran = sleep.timers.run(
                        &*surface.transport,
                        &households,
                        &sleep.fader,
                        &timer.coordinator,
                    );
                    match ran {
                        Ok(outcome) => tracing::info!(
                            "sleep timer in {}'s group: {}",
                            timer.room,
                            match outcome {
                                SleepOutcome::Paused => "faded out and paused",
                                SleepOutcome::Cancelled => "cancelled during its fade",
                                SleepOutcome::Interrupted =>
                                    "ended by a volume change during its fade",
                                SleepOutcome::Gone => "already cancelled",
                            }
                        ),
                        Err(e) => tracing::warn!("sleep timer in {}'s group: {e}", timer.room),
                    }
                });
                Ran::Fading { zone }
            })
            .collect()
    }

    fn run_schedules<Tz: TimeZone>(&self, now: DateTime<Utc>, tz: &Tz) -> Vec<Ran> {
        let schedules = match self.surface.load_schedules() {
            Ok(schedules) => schedules,
            Err(f) => {
                if f.code != ErrorCode::NotImplemented {
                    tracing::warn!("schedules not read: {}", f.detail);
                }
                return Vec::new();
            }
        };
        let mut ran = Vec::new();
        for (id, tick) in core_schedule::due(&schedules, now, tz, self.grace) {
            // Recorded first, so a run can never happen twice.
            match self
                .surface
                .with_store(|s| s.mark_schedule_fired(id, tick.at().timestamp()))
            {
                Ok(Some(true)) => {}
                Ok(_) => continue,
                Err(f) => {
                    tracing::warn!("schedule #{id} not recorded, so not run: {}", f.detail);
                    continue;
                }
            }
            let Some(schedule) = schedules.iter().find(|s| s.id == id).cloned() else {
                continue;
            };
            let what = summary(&schedule.action);
            match tick {
                Tick::Fire { at } => {
                    let surface = Arc::clone(&self.surface);
                    self.spawn("fsonos-schedule", move || match fire(&surface, &schedule) {
                        Ok(done) => tracing::info!("schedule #{id} ({what}): {done}"),
                        Err(f) => tracing::warn!("schedule #{id} ({what}): {}", f.cli_text()),
                    });
                    ran.push(Ran::Fired { id, at });
                }
                Tick::Skip { at } => {
                    tracing::warn!(
                        "schedule #{id} ({what}) skipped its run at {at}: more than {} late",
                        span(self.grace)
                    );
                    ran.push(Ran::Skipped { id, at });
                }
            }
        }
        ran
    }

    fn spawn(&self, name: &str, work: impl FnOnce() + Send + 'static) {
        match thread::Builder::new().name(name.into()).spawn(work) {
            Ok(handle) => {
                let mut runs = self.runs.lock().unwrap_or_else(PoisonError::into_inner);
                runs.retain(|h| !h.is_finished());
                runs.push(handle);
            }
            Err(e) => tracing::warn!("{name} not started: {e}"),
        }
    }

    /// Wait up to `timeout` for the runs started so far; whether all ended.
    pub fn wait(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let mut runs = self.runs.lock().unwrap_or_else(PoisonError::into_inner);
                let (done, pending): (Vec<_>, Vec<_>) = std::mem::take(&mut *runs)
                    .into_iter()
                    .partition(JoinHandle::is_finished);
                *runs = pending;
                for handle in done {
                    let _ = handle.join();
                }
                if runs.is_empty() {
                    return true;
                }
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Stop gracefully: a fade in flight stops and puts its group's volumes
    /// back (the speaker's own timer still pauses the group a little later),
    /// and the runs started are waited for, up to `timeout`.
    pub fn shut_down(&self, timeout: Duration) {
        if let Some(sleep) = &self.surface.sleep {
            let fading: Vec<_> = sleep
                .timers
                .all()
                .into_iter()
                .filter(|t| t.fading)
                .collect();
            if !fading.is_empty() {
                let households = self.surface.households().unwrap_or_default();
                let now = self.surface.clock.now().to_utc();
                for timer in fading {
                    // Re-arming stops the fade; its run puts the volumes back.
                    let rearmed = sleep.timers.extend(
                        &*self.surface.transport,
                        &households,
                        &sleep.fader,
                        &timer.coordinator,
                        Duration::ZERO,
                        now,
                    );
                    if let Err(e) = rearmed {
                        tracing::warn!("sleep fade in {}'s group not stopped: {e}", timer.room);
                    }
                }
            }
        }
        if !self.wait(timeout) {
            tracing::warn!("scheduled runs still going at shutdown");
        }
    }

    /// Tick every [`TICK`] on a thread of its own until `stop` is set, then
    /// [`Self::shut_down`].
    pub fn start(self: &Arc<Self>, stop: Arc<AtomicBool>) -> std::io::Result<JoinHandle<()>> {
        let scheduler = Arc::clone(self);
        thread::Builder::new()
            .name("fsonos-scheduler".into())
            .spawn(move || {
                let mut next = Instant::now();
                while !stop.load(Ordering::Acquire) {
                    if Instant::now() >= next {
                        scheduler.tick();
                        next = (next + TICK).max(Instant::now());
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                scheduler.shut_down(SHUTDOWN_WAIT);
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::surface::Survey;
    use crate::surface::testing::Canned;
    use crate::zones::fixtures::households;
    use fsonos_core::clock::FakeClock;
    use fsonos_core::policy::Policy;
    use fsonos_core::store::MemStore;

    const MIN: fn(u64) -> Duration = Duration::from_mins;

    #[test]
    fn durations_read_as_people_type_them() {
        assert_eq!(parse_duration("45m"), Some(MIN(45)));
        assert_eq!(parse_duration("45"), Some(MIN(45)));
        assert_eq!(parse_duration("1h30m"), Some(MIN(90)));
        assert_eq!(parse_duration(" 1h 30m "), Some(MIN(90)));
        assert_eq!(parse_duration("90s"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration("2H"), Some(MIN(120)));
        for bad in ["", "0", "0m", "m", "45x", "1h30", "-5m", "1.5h"] {
            assert_eq!(parse_duration(bad), None, "{bad:?}");
        }
        assert_eq!(span(MIN(90)), "1h30m");
        assert_eq!(span(Duration::from_secs(90)), "1m30s");
        assert_eq!(span(Duration::ZERO), "0s");
    }

    fn sleep_req(duration: Option<&str>, extend: bool, cancel: bool) -> SleepRequest {
        SleepRequest {
            zone: "Patio".into(),
            duration: duration.map(Into::into),
            extend,
            cancel,
        }
    }

    #[test]
    fn a_sleep_request_sets_extends_or_cancels() {
        let change = |d, e, c| sleep_req(d, e, c).change();
        assert_eq!(
            change(Some("45m"), false, false),
            Ok(SleepChange::Set(MIN(45)))
        );
        assert_eq!(
            change(Some("15"), true, false),
            Ok(SleepChange::Extend(MIN(15)))
        );
        assert_eq!(change(None, false, true), Ok(SleepChange::Cancel));
        for (d, e, c) in [
            (None, false, false),
            (None, true, false),
            (Some("45m"), false, true),
            (None, true, true),
            (Some("soon"), false, false),
            (Some("24h"), false, false),
        ] {
            let err = change(d, e, c).unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidArgument, "{d:?} {e} {c}");
        }
    }

    fn sched(action: &str) -> ScheduleRequest {
        ScheduleRequest {
            when: "daily 07:30".into(),
            action: action.into(),
            ..ScheduleRequest::default()
        }
    }

    #[test]
    fn a_schedule_request_names_one_action_and_only_its_fields() {
        let dj = ScheduleRequest {
            zone: Some(" Kitchen ".into()),
            mood: Some("Bright".into()),
            ..sched("dj-start")
        };
        assert_eq!(
            dj.action(),
            Ok(ScheduleAction::DjStart {
                room: "Kitchen".into(),
                mood: Some("bright".into())
            })
        );
        let pause = ScheduleRequest {
            zone: Some("Patio".into()),
            fade_secs: Some(30),
            ..sched("pause")
        };
        assert_eq!(
            pause.action(),
            Ok(ScheduleAction::Pause {
                room: "Patio".into(),
                fade_secs: 30
            })
        );
        let volume = ScheduleRequest {
            zone: Some("Patio".into()),
            volume: Some(20),
            ..sched("volume")
        };
        assert_eq!(
            volume.action(),
            Ok(ScheduleAction::Volume {
                room: "Patio".into(),
                level: 20
            })
        );
        let scene = ScheduleRequest {
            scene: Some("dinner".into()),
            ..sched("apply_scene")
        };
        assert_eq!(
            scene.action(),
            Ok(ScheduleAction::ApplyScene {
                name: "dinner".into()
            })
        );
        for bad in [
            sched("dj_start"),
            ScheduleRequest {
                volume: Some(101),
                ..volume.clone()
            },
            ScheduleRequest {
                volume: None,
                ..volume.clone()
            },
            ScheduleRequest {
                mood: Some("calm".into()),
                ..volume.clone()
            },
            ScheduleRequest {
                zone: Some("Patio".into()),
                ..scene.clone()
            },
            ScheduleRequest {
                fade_secs: Some(601),
                ..pause.clone()
            },
            sched("reboot"),
        ] {
            assert_eq!(
                bad.action().unwrap_err().code,
                ErrorCode::InvalidArgument,
                "{bad:?}"
            );
        }
    }

    #[test]
    fn a_creator_key_names_its_client_again() {
        for client in [
            Client::Cli,
            Client::McpStdio,
            Client::LoopbackHttp,
            Client::Unknown,
            Client::Tailnet("someone@example.com".into()),
            Client::Tailnet("tag:agent".into()),
        ] {
            assert_eq!(client_of(client.key()), client);
        }
    }

    const T0: &str = "2026-10-08T21:00:00-04:00";

    /// A surface over the fixture households at `T0`, with a store; the
    /// canned speakers answer every action with `out_args`.
    fn surface(out_args: &'static str) -> (Arc<Surface>, Arc<Mutex<Vec<String>>>, Arc<FakeClock>) {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let canned = Canned {
            out_args,
            sent: Arc::clone(&sent),
        };
        let houses = households();
        let survey: Survey = Box::new(move |_| Ok(houses.clone()));
        let clock = Arc::new(FakeClock::new(DateTime::parse_from_rfc3339(T0).unwrap()));
        let shared = SharedClock(Arc::clone(&clock));
        let s = Surface::new(
            Box::new(canned),
            survey,
            Policy::default(),
            Box::new(shared),
        )
        .with_action_log(Box::new(MemStore::default()), "test");
        (Arc::new(s), sent, clock)
    }

    struct SharedClock(Arc<FakeClock>);

    impl fsonos_core::clock::Clock for SharedClock {
        fn now(&self) -> DateTime<FixedOffset> {
            self.0.now()
        }
    }

    fn sent(log: &Mutex<Vec<String>>) -> Vec<String> {
        std::mem::take(&mut *log.lock().unwrap())
    }

    #[test]
    fn without_the_daemon_a_sleep_timer_is_the_speakers_own() {
        let (s, log, _) =
            surface("<RemainingSleepTimerDuration>0:10:00</RemainingSleepTimerDuration>");
        let set = s
            .set_sleep_timer(&Client::Cli, &sleep_req(Some("45m"), false, false))
            .unwrap();
        assert!(!set.fades && set.changed);
        assert_eq!(set.remaining_secs, Some(45 * 60));
        assert_eq!(set.ends.as_deref(), Some("2026-10-08T21:45:00-04:00"));
        assert!(
            set.done
                .starts_with("Patio's group pauses in 45m (at 21:45)"),
            "{}",
            set.done
        );
        assert_eq!(sent(&log), ["ConfigureSleepTimer"]);

        // Extending reads what the speaker has left (10 minutes here).
        let extended = s
            .set_sleep_timer(&Client::Cli, &sleep_req(Some("15m"), true, false))
            .unwrap();
        assert_eq!(extended.remaining_secs, Some(25 * 60));
        assert_eq!(
            sent(&log),
            ["GetRemainingSleepTimerDuration", "ConfigureSleepTimer"]
        );

        let cancelled = s
            .set_sleep_timer(&Client::Cli, &sleep_req(None, false, true))
            .unwrap();
        assert!(cancelled.changed && cancelled.ends.is_none());

        let listed = s.sleep_timers(&Client::Cli, Some("Patio")).unwrap();
        assert_eq!(listed.len(), 1);
        assert!(!listed[0].fades);

        // Every change is in the action log.
        let logged = s
            .recent_actions(&Client::Cli, &fsonos_core::store::ActionFilter::default())
            .unwrap();
        assert_eq!(logged.len(), 3);
        assert!(
            logged
                .iter()
                .all(|a| a.action.intent.starts_with("set_sleep_timer: Patio"))
        );
    }

    #[test]
    fn without_a_timer_there_is_nothing_to_extend_or_cancel() {
        let (s, log, _) = surface("<RemainingSleepTimerDuration></RemainingSleepTimerDuration>");
        let err = s
            .set_sleep_timer(&Client::Cli, &sleep_req(Some("15m"), true, false))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument);
        let cancelled = s
            .set_sleep_timer(&Client::Cli, &sleep_req(None, false, true))
            .unwrap();
        assert!(!cancelled.changed);
        assert_eq!(cancelled.done, "Patio's group had no sleep timer");
        assert_eq!(
            sent(&log),
            [
                "GetRemainingSleepTimerDuration",
                "GetRemainingSleepTimerDuration"
            ]
        );
        assert!(s.sleep_timers(&Client::Cli, None).unwrap().is_empty());
        let denied = s
            .set_sleep_timer(&Client::Unknown, &sleep_req(Some("5m"), false, false))
            .unwrap_err();
        assert_eq!(denied.code, ErrorCode::PolicyDenied);
    }

    fn add(s: &Surface, when: &str, req: ScheduleRequest) -> Result<ScheduleDto, Failure> {
        s.add_schedule(
            &Client::Tailnet("tag:agent".into()),
            &ScheduleRequest {
                when: when.into(),
                ..req
            },
        )
    }

    fn pause_patio() -> ScheduleRequest {
        ScheduleRequest {
            zone: Some("Patio".into()),
            ..sched("pause")
        }
    }

    #[test]
    fn a_schedule_fires_once_as_its_creator_and_a_late_run_is_skipped() {
        let (s, log, clock) = surface("");
        let t0 = clock.now().to_utc();
        let once = add(&s, "in 10m", pause_patio()).unwrap();
        assert_eq!(once.creator, "tag:agent");
        assert_eq!(once.summary, "pause Patio");
        let at = t0 + TimeDelta::minutes(10);
        assert_eq!(once.next, Some(at.with_timezone(&Local).to_rfc3339()));
        let late = add(&s, "in 20m", pause_patio()).unwrap();

        let scheduler = Scheduler::new(&s);
        assert!(
            scheduler
                .tick_at(t0 + TimeDelta::minutes(5), &Local)
                .is_empty()
        );
        let fired = scheduler.tick_at(at + TimeDelta::seconds(1), &Local);
        assert_eq!(fired, [Ran::Fired { id: once.id, at }]);
        assert!(scheduler.wait(Duration::from_secs(5)));
        // (Besides the before-state the action log reads first.)
        let pauses = sent(&log).iter().filter(|a| *a == "Pause").count();
        assert_eq!(pauses, 1);
        // Recorded before it ran: the next tick does not run it again.
        assert!(
            scheduler
                .tick_at(at + TimeDelta::seconds(2), &Local)
                .is_empty()
        );

        // Twenty minutes past its time is past the grace: skipped.
        let skipped = scheduler.tick_at(t0 + TimeDelta::minutes(40), &Local);
        assert_eq!(
            skipped,
            [Ran::Skipped {
                id: late.id,
                at: t0 + TimeDelta::minutes(20)
            }]
        );
        assert!(sent(&log).is_empty());

        // The run is logged as the creator's.
        let logged = s
            .recent_actions(&Client::Cli, &fsonos_core::store::ActionFilter::default())
            .unwrap();
        assert!(logged.iter().any(|a| a.action.client == "tag:agent"
            && a.action.intent.starts_with("pause:")
            && a.action.result.starts_with("paused Patio's group")));
        let listed = s.schedules(&Client::Cli).unwrap();
        assert!(
            listed
                .iter()
                .all(|d| d.next.is_none() && d.last_fired.is_some())
        );
    }

    #[test]
    fn schedules_pause_resume_and_go() {
        let (s, _, clock) = surface("");
        let t0 = clock.now().to_utc();
        let weekly = add(
            &s,
            "daily 21:30",
            ScheduleRequest {
                zone: Some("Patio".into()),
                volume: Some(15),
                ..sched("volume")
            },
        )
        .unwrap();
        assert_eq!(weekly.when, "daily 21:30");
        assert_eq!(weekly.summary, "set Patio's volume to 15");
        let paused = s.pause_schedule(&Client::Cli, weekly.id, true).unwrap();
        assert!(!paused.enabled && paused.next.is_none());
        let scheduler = Scheduler::new(&s);
        assert!(
            scheduler
                .tick_at(t0 + TimeDelta::hours(1), &Local)
                .is_empty()
        );
        let resumed = s.pause_schedule(&Client::Cli, weekly.id, false).unwrap();
        assert!(resumed.enabled && resumed.next.is_some());
        let removed = s.remove_schedule(&Client::Cli, weekly.id).unwrap();
        assert_eq!(removed.id, weekly.id);
        assert!(s.schedules(&Client::Cli).unwrap().is_empty());
        let gone = s.remove_schedule(&Client::Cli, weekly.id).unwrap_err();
        assert_eq!(gone.code, ErrorCode::UnknownSchedule);
    }

    #[test]
    fn a_schedule_is_checked_when_it_is_added() {
        let (s, _, _) = surface("");
        let code = |r: Result<ScheduleDto, Failure>| r.unwrap_err().code;
        assert_eq!(
            code(add(&s, "someday", pause_patio())),
            ErrorCode::InvalidArgument
        );
        assert_eq!(
            code(add(&s, "2020-01-01T00:00:00Z", pause_patio())),
            ErrorCode::InvalidArgument
        );
        let attic = ScheduleRequest {
            zone: Some("Attic".into()),
            ..sched("pause")
        };
        assert_eq!(code(add(&s, "in 5m", attic)), ErrorCode::UnknownRoom);
        let scene = ScheduleRequest {
            scene: Some("dinner".into()),
            ..sched("apply_scene")
        };
        assert_eq!(
            code(add(&s, "in 5m", scene.clone())),
            ErrorCode::UnknownScene
        );
        // Once saved, it can be scheduled.
        let dinner = fsonos_core::scenes::Scene {
            name: "dinner".into(),
            groups: vec![fsonos_core::scenes::SceneGroup {
                coordinator: "Patio".into(),
                members: Vec::new(),
                source: fsonos_core::scenes::SceneSource::default(),
                playing: false,
            }],
            volumes: std::collections::BTreeMap::new(),
            mutes: std::collections::BTreeMap::new(),
        };
        s.with_store(|st| {
            fsonos_core::scenes::save(st, &dinner, 0)
                .map_err(|e| fsonos_core::store::StoreError::Backend(e.to_string()))
        })
        .unwrap();
        let scheduled = add(&s, "in 5m", scene).unwrap();
        assert_eq!(scheduled.summary, "apply the scene dinner");
        // A mood is checked against the DJ's, and this surface has no DJ.
        let bright = ScheduleRequest {
            zone: Some("Patio".into()),
            mood: Some("bright".into()),
            ..sched("dj_start")
        };
        assert_eq!(code(add(&s, "in 5m", bright)), ErrorCode::NotImplemented);
        // A schedule may only do what its creator may.
        let denied = s
            .add_schedule(
                &Client::Unknown,
                &ScheduleRequest {
                    when: "in 5m".into(),
                    ..pause_patio()
                },
            )
            .unwrap_err();
        assert_eq!(denied.code, ErrorCode::PolicyDenied);
        // Only the scene's schedule was added.
        let listed = s.schedules(&Client::Cli).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].scene.as_deref(), Some("dinner"));
    }

    #[test]
    fn without_a_store_there_are_no_schedules() {
        let houses = households();
        let survey: Survey = Box::new(move |_| Ok(houses.clone()));
        let canned = Canned {
            out_args: "",
            sent: Arc::default(),
        };
        let s = Arc::new(Surface::new(
            Box::new(canned),
            survey,
            Policy::default(),
            Box::new(fsonos_core::clock::SystemClock),
        ));
        assert_eq!(
            s.schedules(&Client::Cli).unwrap_err().code,
            ErrorCode::NotImplemented
        );
        assert!(Scheduler::new(&s).tick().is_empty());
    }
}
