//! `dj_rehearse` in the DJ: the set it would play in a group, planned with
//! fsonos-spotify's rehearsal from what a top-up there reads (the group's
//! history, the works its feed has queued ahead, the house's plays and
//! queues), with the steering in effect at each pick's time: the group's
//! session while it lasts, then the time-of-day program, or the mood and
//! constraints the rehearsal names instead.

use chrono::{DateTime, Datelike, FixedOffset, Timelike};
use fsonos_api::Failure;
use fsonos_api::dj::DjSpeakers;
use fsonos_api::surface::dj_rehearse::{
    DjRehearsal, DjRehearsalDto, DjRehearsalHourDto, DjRehearsalSummaryDto, DjRehearsedWorkDto,
    RehearsalLength,
};
use fsonos_core::clock::Clock;
use fsonos_core::store::Store;
use fsonos_spotify::dj::DjConfig;
use fsonos_spotify::feed::{Planning, QueueFeed};
use fsonos_spotify::rehearse::{Rehearsed, Span, summarize};
use fsonos_spotify::steer::{DjSession, Steer, steering};

use super::{State, failure, queued_elsewhere, room};
use crate::dj_view::{self, steer_failure};

/// The steering at a time of the rehearsal.
type Steering<'a> = Box<dyn Fn(DateTime<FixedOffset>) -> Option<Steer> + 'a>;

/// The set `state`'s DJ would play in the group `at` names (see the module
/// docs).
pub(super) fn rehearse(
    state: &State,
    at: DjSpeakers<'_>,
    store: &dyn Store,
    rehearsal: &DjRehearsal,
    clock: &dyn Clock,
) -> Result<DjRehearsalDto, Failure> {
    let now = clock.now();
    let key = at.coordinator.0.clone();
    let steering_at: Steering<'_> = if let Some((mood, constraints)) = &rehearsal.steer {
        let session = DjSession {
            zone: key,
            mood: mood.clone(),
            constraints: dj_view::to_dj(constraints)?,
            expires_at: None,
        };
        let steer = session.steer(&state.moods, None).map_err(|e| {
            steer_failure(
                e,
                "These constraints clash with the mood's own: change one, or pick another mood.",
            )
        })?;
        Box::new(move |_| steer.clone())
    } else {
        // The steering now: a session whose bounds clash says so here.
        steering(
            store,
            &state.moods,
            &key,
            now.timestamp(),
            Some((now.weekday(), now.time())),
        )
        .map_err(|e| {
            steer_failure(
                e,
                "Fix the zone's DJ steering: its bounds clash with the mood's.",
            )
        })?;
        Box::new(move |t: DateTime<FixedOffset>| {
            steering(
                store,
                &state.moods,
                &key,
                t.timestamp(),
                Some((t.weekday(), t.time())),
            )
            .ok()
            .flatten()
        })
    };
    let elsewhere = queued_elsewhere(&state.feeds, at.coordinator, now.timestamp());
    let plan = Planning {
        pool: &state.pool,
        steer: None,
        feedback: state.feedback.as_ref(),
        now: now.timestamp(),
        local_hour: u8::try_from(now.hour()).ok(),
        queued_elsewhere: &elsewhere,
    };
    let fresh;
    let feed = if let Some(feed) = state.feeds.get(at.coordinator) {
        feed
    } else {
        fresh = QueueFeed::new(at.coordinator, DjConfig::default(), rehearsal.seed);
        &fresh
    };
    let span = match rehearsal.length {
        RehearsalLength::Works(n) => Span::Works(usize::try_from(n).unwrap_or(usize::MAX)),
        RehearsalLength::Hours(h) => Span::Minutes(h.saturating_mul(60)),
    };
    let set = feed
        .rehearse(
            store,
            plan,
            rehearsal.start,
            span,
            &*steering_at,
            rehearsal.seed,
        )
        .map_err(failure)?;
    Ok(dto(room(at), rehearsal, &set))
}

fn dto(zone: String, rehearsal: &DjRehearsal, set: &[Rehearsed<'_>]) -> DjRehearsalDto {
    let summary = summarize(set);
    DjRehearsalDto {
        zone,
        from: rehearsal.start.to_rfc3339(),
        seed: rehearsal.seed,
        works: set
            .iter()
            .map(|r| DjRehearsedWorkDto {
                at: r.at.to_rfc3339(),
                work: dj_view::planned(&r.planned),
                genre: r.planned.work.genre().map(str::to_owned),
                energy: r.planned.work.energy(),
                energy_target: r.energy_target,
                mood: r.mood.clone(),
                explicit: r.planned.movements.iter().any(|m| m.explicit),
            })
            .collect(),
        summary: DjRehearsalSummaryDto {
            works: count(summary.works),
            minutes: summary.minutes,
            distinct_artists: count(summary.distinct_artists),
            explicit: count(summary.explicit),
            hours: summary
                .hours
                .iter()
                .map(|h| DjRehearsalHourDto {
                    from: h.from.to_rfc3339(),
                    works: count(h.works),
                    artist_entropy: hundredths(h.artist_entropy),
                    genre_entropy: hundredths(h.genre_entropy),
                    energy: h.energy,
                    energy_target: h.energy_target,
                })
                .collect(),
        },
    }
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Bits to two decimals: more says nothing about a set's variety.
fn hundredths(bits: f64) -> f64 {
    (bits * 100.0).round() / 100.0
}
