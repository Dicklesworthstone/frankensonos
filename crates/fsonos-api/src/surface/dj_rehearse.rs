//! `dj_rehearse`: the set the DJ would play in a room's group from a given
//! time, planned offline, with each work's reason and an hour-by-hour
//! summary of how varied it is and how its energy follows the target. It
//! plays, queues and records nothing, so the owner can ask what the DJ would
//! play at 23:00, and a test can judge a whole set.

use chrono::{DateTime, FixedOffset, NaiveTime, TimeDelta};
use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::policy::Client;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

use super::{Surface, no_dj_store, room_view};
use crate::dj::{DjSpeakers, DjWorkDto, SteerConstraints};
use crate::failure::Failure;
use crate::plan::resolve;

/// How many works a rehearsal plans when not told.
pub const DEFAULT_WORKS: u32 = 12;
/// The most works one rehearsal plans.
pub const MAX_WORKS: u32 = 500;
/// The most hours one rehearsal covers.
pub const MAX_HOURS: u32 = 48;

/// `POST /dj/rehearse` body / the `dj_rehearse` tool / `fsonos dj rehearse`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DjRehearseRequest {
    /// A room: its group's DJ, steering and history.
    pub zone: String,
    /// How many works, 1 to 500 [default: 12].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub works: Option<u32>,
    /// Instead, the works that start within this many hours, 1 to 48.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hours: Option<u32>,
    /// When the set starts: `HH:MM` (its next time, in the house's time
    /// zone) or an RFC 3339 time [default: now].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    /// Rehearse as if the group were steered by this mood (`GET /dj/moods`
    /// lists them), in place of its steering and the time-of-day program.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mood: Option<String>,
    /// Constraints laid over it, likewise only for the rehearsal.
    #[serde(default, skip_serializing_if = "SteerConstraints::is_empty")]
    pub constraints: SteerConstraints,
    /// The same seed rehearses the same set [default: 1].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
}

/// How long a rehearsal runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RehearsalLength {
    Works(u32),
    /// The works that start within this many hours.
    Hours(u32),
}

/// A rehearsal the surface checked, for [`crate::dj::DjEngine::rehearse`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DjRehearsal {
    pub start: DateTime<FixedOffset>,
    pub length: RehearsalLength,
    /// A mood (lowercase) and constraints to rehearse under in place of the
    /// group's steering; `None`: the group's own, as it changes through the
    /// set.
    pub steer: Option<(Option<String>, SteerConstraints)>,
    pub seed: u64,
}

impl DjRehearseRequest {
    /// The rehearsal this asks for, with the house's clock at `now`.
    ///
    /// # Errors
    /// `INVALID_ARGUMENT`: both works and hours, either out of range, or an
    /// `at` that is neither `HH:MM` nor RFC 3339.
    pub fn rehearsal(&self, now: DateTime<FixedOffset>) -> Result<DjRehearsal, Failure> {
        let length = match (self.works, self.hours) {
            (Some(_), Some(_)) => return Err(Failure::invalid("give works or hours, not both")),
            (Some(n), None) if (1..=MAX_WORKS).contains(&n) => RehearsalLength::Works(n),
            (None, Some(h)) if (1..=MAX_HOURS).contains(&h) => RehearsalLength::Hours(h),
            (Some(n), None) => {
                return Err(Failure::invalid(format!(
                    "works must be 1 to {MAX_WORKS}, got {n}"
                )));
            }
            (None, Some(h)) => {
                return Err(Failure::invalid(format!(
                    "hours must be 1 to {MAX_HOURS}, got {h}"
                )));
            }
            (None, None) => RehearsalLength::Works(DEFAULT_WORKS),
        };
        let start = match self.at.as_deref().map(str::trim) {
            None | Some("") => now,
            Some(at) => start_at(at, now)?,
        };
        let steer = (self.mood.is_some() || !self.constraints.is_empty()).then(|| {
            (
                self.mood.as_deref().map(str::to_lowercase),
                self.constraints.clone(),
            )
        });
        Ok(DjRehearsal {
            start,
            length,
            steer,
            seed: self.seed.unwrap_or(1),
        })
    }
}

/// `at` as a start: `HH:MM` is its next time from `now` (today, or tomorrow
/// once it has passed), in `now`'s offset; else RFC 3339.
fn start_at(at: &str, now: DateTime<FixedOffset>) -> Result<DateTime<FixedOffset>, Failure> {
    if let Ok(time) = NaiveTime::parse_from_str(at, "%H:%M") {
        let today = now
            .date_naive()
            .and_time(time)
            .and_local_timezone(*now.offset())
            .single()
            .unwrap_or(now);
        return Ok(if today < now {
            today + TimeDelta::days(1)
        } else {
            today
        });
    }
    DateTime::parse_from_rfc3339(at).map_err(|_| {
        Failure::invalid(format!("at must be HH:MM or an RFC 3339 time, got {at:?}"))
            .with_hint("For example 23:00, or 2026-10-18T23:00:00-04:00.")
    })
}

/// `POST /dj/rehearse` / the `dj_rehearse` tool / `fsonos dj rehearse`: the
/// set the DJ would play.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DjRehearsalDto {
    /// The zone: its coordinator's room.
    pub zone: String,
    /// When the set starts, RFC 3339 in the house's time zone.
    pub from: String,
    /// Rehearsing again with this seed gives the same set.
    pub seed: u64,
    pub works: Vec<DjRehearsedWorkDto>,
    pub summary: DjRehearsalSummaryDto,
}

/// One work of a rehearsal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DjRehearsedWorkDto {
    /// When it would start, RFC 3339.
    pub at: String,
    /// The work, with its length and why the DJ would choose it.
    pub work: DjWorkDto,
    /// Its genre, when the library has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub genre: Option<String>,
    /// Its energy, 0-100.
    pub energy: u8,
    /// The energy the DJ aimed for when it chose it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub energy_target: Option<u8>,
    /// The mood steering that pick, if one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mood: Option<String>,
    /// A movement of it is marked explicit.
    pub explicit: bool,
}

/// What a rehearsal adds up to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DjRehearsalSummaryDto {
    pub works: u32,
    pub minutes: u32,
    /// Distinct artists: a classical work's composer, a song's lead artist.
    pub distinct_artists: u32,
    /// Works with an explicit movement.
    pub explicit: u32,
    /// Hour by hour, in order.
    pub hours: Vec<DjRehearsalHourDto>,
}

/// The works of a rehearsal that start in one hour.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DjRehearsalHourDto {
    /// When the hour's first work starts, RFC 3339.
    pub from: String,
    pub works: u32,
    /// Shannon entropy, in bits, of the hour's artists: 0 when one artist
    /// plays it, log2(works) when each work has its own.
    pub artist_entropy: f64,
    /// The same over their genres.
    pub genre_entropy: f64,
    /// Their mean energy, 0-100.
    pub energy: u8,
    /// The energy the DJ aimed for as the hour began.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub energy_target: Option<u8>,
}

/// `HH:MM` of an RFC 3339 time (the text itself when it is not one).
fn clock_time(at: &str) -> String {
    DateTime::parse_from_rfc3339(at)
        .map_or_else(|_| at.to_owned(), |t| t.format("%H:%M").to_string())
}

impl DjRehearsalDto {
    /// For people: a line per work (when, what, how long, the mood and the
    /// reason), then the hours.
    #[must_use]
    pub fn text(&self) -> String {
        let mut out = format!(
            "What the DJ would play in {}'s group from {} (seed {}):\n",
            self.zone,
            clock_time(&self.from),
            self.seed
        );
        for w in &self.works {
            let minutes = w
                .work
                .minutes
                .map_or_else(String::new, |m| format!(" ({m} min)"));
            let mood = w
                .mood
                .as_deref()
                .map_or_else(String::new, |m| format!(" [{m}]"));
            let _ = writeln!(
                out,
                "  {}  {}: {}{minutes}{mood} - {}",
                clock_time(&w.at),
                w.work.composer,
                w.work.title,
                w.work.reason.summary
            );
        }
        let s = &self.summary;
        let explicit = if s.explicit > 0 {
            format!(", {} explicit", s.explicit)
        } else {
            String::new()
        };
        let _ = writeln!(
            out,
            "{} works, {} min, {} artists{explicit}",
            s.works, s.minutes, s.distinct_artists
        );
        for h in &s.hours {
            let aim = h
                .energy_target
                .map_or_else(String::new, |t| format!(" (aim {t})"));
            let _ = writeln!(
                out,
                "  from {}: {} works, energy {}{aim}, artist variety {:.1} bits, genre {:.1} bits",
                clock_time(&h.from),
                h.works,
                h.energy,
                h.artist_entropy,
                h.genre_entropy
            );
        }
        out
    }
}

impl Surface {
    /// The set the DJ would play in the group `req.zone` plays in
    /// (`dj_rehearse`), planned from that group's history, steering and the
    /// house's plays. Nothing plays, queues or is recorded.
    pub fn dj_rehearse(
        &self,
        client: &Client,
        req: &DjRehearseRequest,
    ) -> Result<DjRehearsalDto, Failure> {
        self.guard(client).authorize("dj_rehearse", true)?;
        let dj = self.dj_engine()?;
        let rehearsal = req.rehearsal(self.clock.now())?;
        let households = self.households()?;
        let aliases = self.aliases();
        let target = resolve(room_view(&households, aliases.as_ref(), client), &req.zone)
            .map_err(|f| self.explain(f))?;
        let at = DjSpeakers {
            transport: &*self.transport,
            households: &households,
            coordinator: &target.coordinator.id,
        };
        self.with_dj_store(|store| Ok(dj.rehearse(at, store, &rehearsal, &*self.clock)))?
            .unwrap_or_else(|| Err(no_dj_store()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::failure::ErrorCode;

    fn now() -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339("2026-10-18T20:30:00-04:00").unwrap()
    }

    fn ask(req: DjRehearseRequest) -> Result<DjRehearsal, Failure> {
        DjRehearseRequest {
            zone: "Kitchen".into(),
            ..req
        }
        .rehearsal(now())
    }

    #[test]
    fn a_bare_request_rehearses_twelve_works_from_now_under_the_groups_steering() {
        let r = ask(DjRehearseRequest::default()).unwrap();
        assert_eq!(r.start, now());
        assert_eq!(r.length, RehearsalLength::Works(DEFAULT_WORKS));
        assert_eq!((r.steer, r.seed), (None, 1));
    }

    #[test]
    fn a_clock_time_is_its_next_time_and_rfc_3339_is_taken_as_given() {
        let at = |at: &str| {
            ask(DjRehearseRequest {
                at: Some(at.into()),
                ..DjRehearseRequest::default()
            })
            .map(|r| r.start.to_rfc3339())
        };
        assert_eq!(at("23:00").unwrap(), "2026-10-18T23:00:00-04:00");
        assert_eq!(
            at("07:15").unwrap(),
            "2026-10-19T07:15:00-04:00",
            "tomorrow"
        );
        assert_eq!(at("20:30").unwrap(), "2026-10-18T20:30:00-04:00", "now");
        assert_eq!(
            at("2026-12-24T18:00:00+01:00").unwrap(),
            "2026-12-24T18:00:00+01:00"
        );
        let wrong = at("tonight").unwrap_err();
        assert_eq!(wrong.code, ErrorCode::InvalidArgument);
        assert!(wrong.detail.contains("tonight"), "{wrong:?}");
    }

    #[test]
    fn works_or_hours_within_bounds_and_a_mood_steers_only_the_rehearsal() {
        let len = |works, hours| {
            ask(DjRehearseRequest {
                works,
                hours,
                ..DjRehearseRequest::default()
            })
            .map(|r| r.length)
        };
        assert_eq!(len(Some(3), None).unwrap(), RehearsalLength::Works(3));
        assert_eq!(len(None, Some(48)).unwrap(), RehearsalLength::Hours(48));
        for wrong in [
            (Some(0), None),
            (Some(501), None),
            (None, Some(49)),
            (Some(3), Some(2)),
        ] {
            assert_eq!(
                len(wrong.0, wrong.1).unwrap_err().code,
                ErrorCode::InvalidArgument,
                "{wrong:?}"
            );
        }
        let calm = ask(DjRehearseRequest {
            mood: Some("Calm".into()),
            seed: Some(9),
            ..DjRehearseRequest::default()
        })
        .unwrap();
        assert_eq!(
            calm.steer,
            Some((Some("calm".into()), SteerConstraints::default()))
        );
        assert_eq!(calm.seed, 9);
    }
}
