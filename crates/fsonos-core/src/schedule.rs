//! Schedules: apply a scene, start the DJ, pause, or set a volume at set
//! times, once or every week.
//!
//! The daemon owns scheduling because Sonos's own alarms can neither start
//! the DJ nor a scene, and differ between S1 and S2. This module is the pure
//! part: [`ScheduleSpec`] parses what people type ("in 45m", an RFC 3339
//! time, "weekdays 07:30", "sat,sun 09:00", "daily 22:30"); [`next_fire`]
//! finds the next run in the house's time zone across DST changes; [`tick`]
//! decides, for one schedule at one moment, whether to run now or skip a run
//! missed by more than the grace period. Recording the run before carrying
//! it out (store `mark_schedule_fired`) is what keeps a run from ever firing
//! twice. A run is carried out by the surface as the client that created the
//! schedule, so a schedule can never do more than its creator may.
//!
//! DST: a wall time the spring-forward gap skips runs at the first minute
//! after the gap; a wall time the fall-back overlap repeats runs once, at
//! its first occurrence.

use crate::store::{Store, StoreError, StoredSchedule};
use chrono::{
    DateTime, Datelike, FixedOffset, MappedLocalTime, NaiveDateTime, NaiveTime, TimeDelta,
    TimeZone, Utc, Weekday,
};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::Duration;

/// How late a run may start before it is skipped instead.
pub const DEFAULT_GRACE: Duration = Duration::from_mins(10);

/// The longest catch-up after downtime, in skipped runs.
const MAX_CATCH_UP: usize = 100_000;

/// Why a schedule spec is not understood.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScheduleError {
    #[error(
        "{0:?} is not a schedule; try \"in 45m\", an RFC 3339 time, \"daily 22:30\", \
         \"weekdays 07:30\" or \"sat,sun 09:00\""
    )]
    Unrecognized(String),
    #[error("{0} has already passed")]
    Past(String),
    #[error("unreadable stored schedule: {0}")]
    Corrupt(String),
    #[error("store error: {0}")]
    Store(String),
}

impl From<StoreError> for ScheduleError {
    fn from(e: StoreError) -> Self {
        Self::Store(e.to_string())
    }
}

/// A set of weekdays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Days(u8);

impl Days {
    pub const DAILY: Self = Self(0x7f);
    pub const WEEKDAYS: Self = Self(0x1f);
    pub const WEEKENDS: Self = Self(0x60);

    #[must_use]
    pub fn of(days: &[Weekday]) -> Self {
        Self(
            days.iter()
                .fold(0, |bits, d| bits | 1 << d.num_days_from_monday()),
        )
    }

    #[must_use]
    pub fn contains(self, day: Weekday) -> bool {
        (self.0 & (1 << day.num_days_from_monday())) != 0
    }
}

/// When a schedule runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduleSpec {
    /// Once, at this instant.
    Once(DateTime<FixedOffset>),
    /// Every week on `days`, at wall time `at` in the house's time zone.
    Weekly { days: Days, at: NaiveTime },
}

const DAY_NAMES: [(&str, Weekday); 7] = [
    ("mon", Weekday::Mon),
    ("tue", Weekday::Tue),
    ("wed", Weekday::Wed),
    ("thu", Weekday::Thu),
    ("fri", Weekday::Fri),
    ("sat", Weekday::Sat),
    ("sun", Weekday::Sun),
];

/// A day by its name or any prefix of it of three letters or more.
fn weekday(word: &str) -> Option<Weekday> {
    let w = word.trim();
    DAY_NAMES
        .iter()
        .find(|(short, _)| w.len() >= 3 && full_name(short).starts_with(w))
        .map(|(_, d)| *d)
}

fn full_name(short: &str) -> &'static str {
    match short {
        "mon" => "monday",
        "tue" => "tuesday",
        "wed" => "wednesday",
        "thu" => "thursday",
        "fri" => "friday",
        "sat" => "saturday",
        _ => "sunday",
    }
}

/// "weekdays", "daily", "sat,sun", "mon-fri", "monday".
fn parse_days(word: &str) -> Option<Days> {
    match word {
        "daily" | "everyday" | "every-day" => return Some(Days::DAILY),
        "weekdays" => return Some(Days::WEEKDAYS),
        "weekends" => return Some(Days::WEEKENDS),
        _ => {}
    }
    let mut days = Vec::new();
    for part in word.split(',') {
        if let Some((from, to)) = part.split_once('-') {
            let (from, to) = (weekday(from)?, weekday(to)?);
            let mut d = from;
            days.push(d);
            while d != to {
                d = d.succ();
                days.push(d);
            }
        } else {
            days.push(weekday(part)?);
        }
    }
    (!days.is_empty()).then(|| Days::of(&days))
}

/// "45m", "2h", "1h30m", "90s", "1d".
fn parse_delay(s: &str) -> Option<TimeDelta> {
    let mut total = TimeDelta::zero();
    let mut number = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            number.push(c);
            continue;
        }
        let n: i64 = number.parse().ok()?;
        number.clear();
        total += match c {
            'd' => TimeDelta::try_days(n)?,
            'h' => TimeDelta::try_hours(n)?,
            'm' => TimeDelta::try_minutes(n)?,
            's' => TimeDelta::try_seconds(n)?,
            _ => return None,
        };
    }
    (number.is_empty() && total > TimeDelta::zero()).then_some(total)
}

impl ScheduleSpec {
    /// What a person or agent typed, at `now`: a relative time is fixed
    /// here, and a one-shot time that has passed is refused.
    pub fn parse(input: &str, now: DateTime<FixedOffset>) -> Result<Self, ScheduleError> {
        let s = input.trim().to_lowercase();
        if let Some(delay) = s
            .strip_prefix("in ")
            .and_then(|d| parse_delay(&d.replace(' ', "")))
        {
            return Ok(Self::Once(now + delay));
        }
        let spec = Self::from_canonical(&s)
            .map_err(|_| ScheduleError::Unrecognized(input.trim().to_string()))?;
        match spec {
            Self::Once(at) if at <= now => Err(ScheduleError::Past(at.to_rfc3339())),
            spec => Ok(spec),
        }
    }

    /// A spec as [`fmt::Display`] writes it (and the RFC 3339 and day forms
    /// [`Self::parse`] reads), with no notion of now.
    pub fn from_canonical(s: &str) -> Result<Self, ScheduleError> {
        let s = s.trim();
        let bare = s
            .strip_prefix("once ")
            .or_else(|| s.strip_prefix("at "))
            .unwrap_or(s);
        if let Ok(at) = DateTime::parse_from_rfc3339(&bare.to_uppercase()) {
            return Ok(Self::Once(at));
        }
        let corrupt = || ScheduleError::Corrupt(s.to_string());
        let (days, time) = s.rsplit_once(' ').ok_or_else(corrupt)?;
        let days = parse_days(&days.replace(' ', "").to_lowercase()).ok_or_else(corrupt)?;
        let at = NaiveTime::parse_from_str(time, "%H:%M").map_err(|_| corrupt())?;
        Ok(Self::Weekly { days, at })
    }
}

impl fmt::Display for ScheduleSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Once(at) => write!(f, "once {}", at.to_rfc3339()),
            Self::Weekly { days, at } => {
                let at = at.format("%H:%M");
                match *days {
                    Days::DAILY => write!(f, "daily {at}"),
                    Days::WEEKDAYS => write!(f, "weekdays {at}"),
                    Days::WEEKENDS => write!(f, "weekends {at}"),
                    days => {
                        let names: Vec<&str> = DAY_NAMES
                            .iter()
                            .filter(|(_, d)| days.contains(*d))
                            .map(|(n, _)| *n)
                            .collect();
                        write!(f, "{} {at}", names.join(","))
                    }
                }
            }
        }
    }
}

/// The instant wall time `local` names in `tz`: the first of two in a
/// fall-back overlap, the first minute after a spring-forward gap.
fn resolve<Tz: TimeZone>(tz: &Tz, local: NaiveDateTime) -> Option<DateTime<Utc>> {
    for minutes in 0..=180 {
        match tz.from_local_datetime(&(local + TimeDelta::minutes(minutes))) {
            MappedLocalTime::Single(t) | MappedLocalTime::Ambiguous(t, _) => {
                return Some(t.with_timezone(&Utc));
            }
            MappedLocalTime::None => {}
        }
    }
    None
}

/// The first run of `spec` strictly after `after`, in time zone `tz`.
pub fn next_fire<Tz: TimeZone>(
    spec: &ScheduleSpec,
    after: DateTime<Utc>,
    tz: &Tz,
) -> Option<DateTime<Utc>> {
    match spec {
        ScheduleSpec::Once(at) => {
            let at = at.with_timezone(&Utc);
            (at > after).then_some(at)
        }
        ScheduleSpec::Weekly { days, at } => {
            let start = after.with_timezone(tz).date_naive();
            (0..=7)
                .filter_map(|d| start.checked_add_days(chrono::Days::new(d)))
                .filter(|date| days.contains(date.weekday()))
                .filter_map(|date| resolve(tz, date.and_time(*at)))
                .find(|t| *t > after)
        }
    }
}

/// What to do about a schedule at a tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tick {
    /// Run it now (it was due at `at`).
    Fire { at: DateTime<Utc> },
    /// The run due at `at` (the latest of any missed) is too late to start:
    /// record it as done without running it.
    Skip { at: DateTime<Utc> },
}

impl Tick {
    /// The run to record as fired, either way.
    #[must_use]
    pub fn at(self) -> DateTime<Utc> {
        match self {
            Self::Fire { at } | Self::Skip { at } => at,
        }
    }
}

/// Whether a schedule created at `created` that last fired at `last_fired`
/// is due at `now`. Runs missed while the daemon was down collapse into the
/// latest one, which fires if it is at most `grace` late and is skipped
/// otherwise; `None` means nothing is due.
pub fn tick<Tz: TimeZone>(
    spec: &ScheduleSpec,
    created: DateTime<Utc>,
    last_fired: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    tz: &Tz,
    grace: Duration,
) -> Option<Tick> {
    let mut at = next_fire(spec, last_fired.unwrap_or(created), tz)?;
    if at > now {
        return None;
    }
    for _ in 0..MAX_CATCH_UP {
        match next_fire(spec, at, tz) {
            Some(next) if next <= now => at = next,
            _ => break,
        }
    }
    let late = (now - at).to_std().unwrap_or_default();
    Some(if late <= grace {
        Tick::Fire { at }
    } else {
        Tick::Skip { at }
    })
}

/// What a schedule does when it runs. Rooms and scenes are by name, as
/// people say them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScheduleAction {
    ApplyScene {
        name: String,
    },
    DjStart {
        room: String,
        #[serde(default)]
        mood: Option<String>,
    },
    /// Pause, fading out over `fade_secs` first.
    Pause {
        room: String,
        #[serde(default)]
        fade_secs: u32,
    },
    Volume {
        room: String,
        level: u8,
    },
}

/// A schedule as stored, read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schedule {
    pub id: i64,
    pub spec: ScheduleSpec,
    pub action: ScheduleAction,
    /// The policy client whose rights its runs have.
    pub creator: String,
    pub enabled: bool,
    pub created: DateTime<Utc>,
    pub last_fired: Option<DateTime<Utc>>,
}

fn instant(secs: i64) -> Result<DateTime<Utc>, ScheduleError> {
    DateTime::from_timestamp(secs, 0).ok_or_else(|| ScheduleError::Corrupt(format!("time {secs}")))
}

impl Schedule {
    pub fn from_stored(stored: &StoredSchedule) -> Result<Self, ScheduleError> {
        Ok(Self {
            id: stored.id,
            spec: ScheduleSpec::from_canonical(&stored.spec)?,
            action: serde_json::from_str(&stored.action)
                .map_err(|e| ScheduleError::Corrupt(format!("action of {}: {e}", stored.id)))?,
            creator: stored.creator.clone(),
            enabled: stored.enabled,
            created: instant(stored.created)?,
            last_fired: stored.last_fired.map(instant).transpose()?,
        })
    }

    /// When it runs next after `now` (ignoring whether it is enabled).
    #[must_use]
    pub fn next<Tz: TimeZone>(&self, now: DateTime<Utc>, tz: &Tz) -> Option<DateTime<Utc>> {
        let after = self.last_fired.map_or(now, |last| last.max(now));
        next_fire(&self.spec, after, tz)
    }
}

/// Add a schedule for `creator`, created at `now`; returns its id.
pub fn add<S: Store + ?Sized>(
    store: &mut S,
    spec: &ScheduleSpec,
    action: &ScheduleAction,
    creator: &str,
    now: DateTime<Utc>,
) -> Result<i64, ScheduleError> {
    let action =
        serde_json::to_string(action).map_err(|e| ScheduleError::Corrupt(e.to_string()))?;
    Ok(store.add_schedule(&StoredSchedule {
        id: 0,
        spec: spec.to_string(),
        action,
        creator: creator.to_string(),
        enabled: true,
        created: now.timestamp(),
        last_fired: None,
    })?)
}

/// Every stored schedule, read.
pub fn load<S: Store + ?Sized>(store: &S) -> Result<Vec<Schedule>, ScheduleError> {
    store
        .schedules()?
        .iter()
        .map(Schedule::from_stored)
        .collect()
}

/// What every enabled schedule should do at `now`: `(id, tick)` for each
/// one that is due.
#[must_use]
pub fn due<Tz: TimeZone>(
    schedules: &[Schedule],
    now: DateTime<Utc>,
    tz: &Tz,
    grace: Duration,
) -> Vec<(i64, Tick)> {
    schedules
        .iter()
        .filter(|s| s.enabled)
        .filter_map(|s| tick(&s.spec, s.created, s.last_fired, now, tz, grace).map(|t| (s.id, t)))
        .collect()
}

/// Record `tick` for schedule `id` before acting on it. Only a `true`
/// answer for a [`Tick::Fire`] means run it now: anything else was already
/// recorded (another tick got there first) or is a skip.
pub fn claim<S: Store + ?Sized>(store: &mut S, id: i64, tick: Tick) -> Result<bool, ScheduleError> {
    let claimed = store.mark_schedule_fired(id, tick.at().timestamp())?;
    Ok(claimed && matches!(tick, Tick::Fire { .. }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    /// A zone with US-style DST in 2026: UTC−5, and UTC−4 from 2026-03-08
    /// 02:00 (clocks jump to 03:00) to 2026-11-01 02:00 (back to 01:00).
    #[derive(Debug, Clone, Copy)]
    struct Eastern;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    impl Eastern {
        fn offset_at(utc_time: &NaiveDateTime) -> FixedOffset {
            let start = utc("2026-03-08T07:00:00Z").naive_utc();
            let end = utc("2026-11-01T06:00:00Z").naive_utc();
            let hours = if (start..end).contains(utc_time) {
                4
            } else {
                5
            };
            FixedOffset::west_opt(hours * 3600).unwrap()
        }
    }

    impl TimeZone for Eastern {
        type Offset = FixedOffset;

        fn from_offset(_: &FixedOffset) -> Self {
            Self
        }

        fn offset_from_local_date(&self, local: &NaiveDate) -> MappedLocalTime<FixedOffset> {
            self.offset_from_local_datetime(&local.and_time(NaiveTime::MIN))
        }

        fn offset_from_local_datetime(
            &self,
            local: &NaiveDateTime,
        ) -> MappedLocalTime<FixedOffset> {
            // Each offset that maps `local` back to itself is a reading of it.
            let readings: Vec<FixedOffset> = [4, 5]
                .into_iter()
                .map(|h| FixedOffset::west_opt(h * 3600).unwrap())
                .filter(|o| Self::offset_at(&(*local - *o)) == *o)
                .collect();
            match readings[..] {
                [] => MappedLocalTime::None,
                [one] => MappedLocalTime::Single(one),
                // The DST reading (UTC−4) is the earlier instant.
                [dst, std] => MappedLocalTime::Ambiguous(dst, std),
                _ => unreachable!(),
            }
        }

        fn offset_from_utc_date(&self, utc: &NaiveDate) -> FixedOffset {
            Self::offset_at(&utc.and_time(NaiveTime::MIN))
        }

        fn offset_from_utc_datetime(&self, utc: &NaiveDateTime) -> FixedOffset {
            Self::offset_at(utc)
        }
    }

    fn at(s: &str) -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339(s).unwrap()
    }

    fn spec(s: &str) -> ScheduleSpec {
        ScheduleSpec::from_canonical(s).unwrap()
    }

    fn local(t: DateTime<Utc>) -> String {
        t.with_timezone(&Eastern)
            .format("%a %Y-%m-%d %H:%M %z")
            .to_string()
    }

    #[test]
    fn specs_parse_and_print_canonically() {
        let now = at("2026-10-07T12:00:00-04:00");
        for (input, canonical) in [
            ("daily 22:30", "daily 22:30"),
            ("Weekdays 07:30", "weekdays 07:30"),
            ("weekends 09:00", "weekends 09:00"),
            ("sat,sun 09:00", "weekends 09:00"),
            ("mon-fri 07:30", "weekdays 07:30"),
            ("mon,wed,fri 06:45", "mon,wed,fri 06:45"),
            ("Monday 07:00", "mon 07:00"),
            ("fri-mon 23:00", "mon,fri,sat,sun 23:00"),
            ("in 45m", "once 2026-10-07T12:45:00-04:00"),
            ("in 1h 30m", "once 2026-10-07T13:30:00-04:00"),
            (
                "2026-10-08T07:30:00-04:00",
                "once 2026-10-08T07:30:00-04:00",
            ),
            ("at 2026-10-08t07:30:00z", "once 2026-10-08T07:30:00+00:00"),
        ] {
            let parsed = ScheduleSpec::parse(input, now).unwrap();
            assert_eq!(parsed.to_string(), canonical, "{input}");
            assert_eq!(
                ScheduleSpec::from_canonical(canonical).unwrap(),
                parsed,
                "{input}"
            );
        }
        for bad in [
            "sometime",
            "daily 25:00",
            "in 0m",
            "in 5x",
            "funday 07:00",
            "weekdays",
        ] {
            assert!(
                matches!(
                    ScheduleSpec::parse(bad, now),
                    Err(ScheduleError::Unrecognized(_))
                ),
                "{bad}"
            );
        }
        assert_eq!(
            ScheduleSpec::parse("2026-10-07T11:00:00-04:00", now),
            Err(ScheduleError::Past("2026-10-07T11:00:00-04:00".into()))
        );
    }

    #[test]
    fn next_fire_walks_the_week_in_local_time() {
        let weekdays = spec("weekdays 07:30");
        // Friday 08:00 local: the next weekday run is Monday.
        let next = next_fire(&weekdays, utc("2026-10-09T12:00:00Z"), &Eastern).unwrap();
        assert_eq!(local(next), "Mon 2026-10-12 07:30 -0400");
        // Exactly at a run: the next one, not the same.
        let again = next_fire(&weekdays, next, &Eastern).unwrap();
        assert_eq!(local(again), "Tue 2026-10-13 07:30 -0400");
        let sunday = spec("sun 09:00");
        let next = next_fire(&sunday, utc("2026-10-11T12:59:00Z"), &Eastern).unwrap();
        assert_eq!(
            local(next),
            "Sun 2026-10-11 09:00 -0400",
            "later the same day"
        );
        let once = spec("once 2026-10-08T07:30:00-04:00");
        assert_eq!(
            next_fire(&once, utc("2026-10-08T11:29:00Z"), &Eastern),
            Some(utc("2026-10-08T11:30:00Z"))
        );
        assert_eq!(
            next_fire(&once, utc("2026-10-08T11:30:00Z"), &Eastern),
            None
        );
    }

    #[test]
    fn dst_gaps_run_after_the_gap_and_overlaps_run_once() {
        // Spring forward: 02:30 does not exist on 2026-03-08.
        let night = spec("daily 02:30");
        let next = next_fire(&night, utc("2026-03-07T12:00:00Z"), &Eastern).unwrap();
        assert_eq!(local(next), "Sun 2026-03-08 03:00 -0400");
        let after = next_fire(&night, next, &Eastern).unwrap();
        assert_eq!(local(after), "Mon 2026-03-09 02:30 -0400");
        // The day's wall clock stays put across the change.
        let morning = spec("daily 07:30");
        let sat = next_fire(&morning, utc("2026-03-07T00:00:00Z"), &Eastern).unwrap();
        let sun = next_fire(&morning, sat, &Eastern).unwrap();
        assert_eq!(
            (local(sat), local(sun)),
            (
                "Sat 2026-03-07 07:30 -0500".into(),
                "Sun 2026-03-08 07:30 -0400".into()
            )
        );
        assert_eq!((sun - sat).num_hours(), 23);

        // Fall back: 01:30 happens twice on 2026-11-01; it runs once.
        let late = spec("daily 01:30");
        let first = next_fire(&late, utc("2026-10-31T12:00:00Z"), &Eastern).unwrap();
        assert_eq!(
            first,
            utc("2026-11-01T05:30:00Z"),
            "the first 01:30 (−04:00)"
        );
        let next = next_fire(&late, first, &Eastern).unwrap();
        assert_eq!(
            local(next),
            "Mon 2026-11-02 01:30 -0500",
            "not the second 01:30"
        );
    }

    #[test]
    fn missed_runs_are_skipped_and_none_fires_twice() {
        let morning = spec("daily 07:30");
        let created = utc("2026-10-01T00:00:00Z");
        let grace = DEFAULT_GRACE;
        let tick_at = |last: Option<&str>, now: &str| {
            tick(&morning, created, last.map(utc), utc(now), &Eastern, grace)
        };
        // Before the first run: nothing.
        assert_eq!(tick_at(None, "2026-10-01T11:29:00Z"), None);
        // On time and a little late: fire.
        assert_eq!(
            tick_at(None, "2026-10-01T11:30:00Z"),
            Some(Tick::Fire {
                at: utc("2026-10-01T11:30:00Z")
            })
        );
        assert_eq!(
            tick_at(None, "2026-10-01T11:40:00Z"),
            Some(Tick::Fire {
                at: utc("2026-10-01T11:30:00Z")
            })
        );
        // Recorded as fired: not again that day.
        assert_eq!(
            tick_at(Some("2026-10-01T11:30:00Z"), "2026-10-01T11:41:00Z"),
            None
        );
        // More than the grace late: skipped (and recorded, so never fired).
        let skip = tick_at(None, "2026-10-01T11:41:00Z").unwrap();
        assert_eq!(
            skip,
            Tick::Skip {
                at: utc("2026-10-01T11:30:00Z")
            }
        );
        assert_eq!(
            tick_at(Some(&skip.at().to_rfc3339()), "2026-10-01T23:00:00Z"),
            None
        );
        // Down for days: the missed runs collapse into the latest, which
        // fires if it is on time.
        assert_eq!(
            tick_at(Some("2026-10-01T11:30:00Z"), "2026-10-05T11:35:00Z"),
            Some(Tick::Fire {
                at: utc("2026-10-05T11:30:00Z")
            })
        );
        assert_eq!(
            tick_at(Some("2026-10-01T11:30:00Z"), "2026-10-05T20:00:00Z"),
            Some(Tick::Skip {
                at: utc("2026-10-05T11:30:00Z")
            })
        );
        // A one-shot runs once.
        let once = spec("once 2026-10-08T07:30:00-04:00");
        let first = tick(
            &once,
            created,
            None,
            utc("2026-10-08T11:31:00Z"),
            &Eastern,
            grace,
        )
        .unwrap();
        assert_eq!(
            first,
            Tick::Fire {
                at: utc("2026-10-08T11:30:00Z")
            }
        );
        assert_eq!(
            tick(
                &once,
                created,
                Some(first.at()),
                utc("2026-10-09T00:00:00Z"),
                &Eastern,
                grace
            ),
            None
        );
    }

    #[test]
    fn stored_schedules_tick_once_per_run_through_the_store() {
        let mut store = crate::store::MemStore::default();
        let created = utc("2026-10-05T00:00:00Z");
        let morning = add(
            &mut store,
            &spec("weekdays 07:30"),
            &ScheduleAction::Volume {
                room: "Kitchen".into(),
                level: 25,
            },
            "tag:assistant",
            created,
        )
        .unwrap();
        let weekend = add(
            &mut store,
            &spec("weekends 09:00"),
            &ScheduleAction::ApplyScene {
                name: "Brunch".into(),
            },
            "cli",
            created,
        )
        .unwrap();
        let all = load(&store).unwrap();
        assert_eq!(all[0].creator, "tag:assistant");
        assert_eq!(
            all[0].next(utc("2026-10-09T12:00:00Z"), &Eastern),
            Some(utc("2026-10-12T11:30:00Z")),
            "Friday after the run: Monday"
        );

        // Monday 07:31 local: the weekday schedule fires, the other waits.
        let now = utc("2026-10-05T11:31:00Z");
        let ticks = due(&all, now, &Eastern, DEFAULT_GRACE);
        assert_eq!(
            ticks,
            [(
                morning,
                Tick::Fire {
                    at: utc("2026-10-05T11:30:00Z")
                }
            )]
        );
        // Two ticks racing for the same run: only one claims it.
        assert!(claim(&mut store, morning, ticks[0].1).unwrap());
        assert!(!claim(&mut store, morning, ticks[0].1).unwrap());
        let all = load(&store).unwrap();
        assert_eq!(due(&all, now, &Eastern, DEFAULT_GRACE), []);

        // A disabled schedule never comes due; a skip is recorded but not run.
        store.set_schedule_enabled(morning, false).unwrap();
        let saturday_late = utc("2026-10-10T20:00:00Z");
        let all = load(&store).unwrap();
        let ticks = due(&all, saturday_late, &Eastern, DEFAULT_GRACE);
        assert_eq!(
            ticks,
            [(
                weekend,
                Tick::Skip {
                    at: utc("2026-10-10T13:00:00Z")
                }
            )]
        );
        assert!(
            !claim(&mut store, weekend, ticks[0].1).unwrap(),
            "skips are not run"
        );
        assert_eq!(
            due(
                &load(&store).unwrap(),
                saturday_late,
                &Eastern,
                DEFAULT_GRACE
            ),
            []
        );
    }

    #[test]
    fn actions_serialize_with_a_kind() {
        let a = ScheduleAction::Pause {
            room: "Bedroom".into(),
            fade_secs: 120,
        };
        let json = serde_json::to_string(&a).unwrap();
        assert_eq!(json, r#"{"kind":"pause","room":"Bedroom","fade_secs":120}"#);
        assert_eq!(serde_json::from_str::<ScheduleAction>(&json).unwrap(), a);
        let dj: ScheduleAction =
            serde_json::from_str(r#"{"kind":"dj_start","room":"Kitchen"}"#).unwrap();
        assert_eq!(
            dj,
            ScheduleAction::DjStart {
                room: "Kitchen".into(),
                mood: None
            }
        );
    }
}
