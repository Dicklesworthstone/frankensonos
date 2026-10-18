//! A rehearsal: the set the DJ would play from a given time, planned offline
//! with each work's reason and a summary, writing nothing. Each pick sees
//! the earlier ones as just played, so the anti-repeat rules apply within
//! the set, and the clock moves on by each work's length, so the time-of-day
//! energy target, the program's moods and a mood's end apply as they would
//! on the night.

use crate::dj::{
    DjConfig, PickContext, PlannedWork, PlayRecord, Rng, WorkPool, energy_target, pick_next,
};
use crate::steer::Steer;
use chrono::{DateTime, FixedOffset, TimeDelta, Timelike};
use serde::Serialize;
use std::collections::{HashMap, HashSet};

/// How long a movement of unknown length is taken to last.
pub const UNKNOWN_MOVEMENT_SECS: u32 = 240;

/// The most works one rehearsal plans.
pub const MAX_WORKS: usize = 500;

/// How far a rehearsal runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Span {
    /// This many works.
    Works(usize),
    /// The works that start within this many minutes.
    Minutes(u32),
}

/// One work of a rehearsal: when it would start, how long it plays, the
/// mood steering it and the energy it aimed for, and the pick with its
/// reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rehearsed<'p> {
    pub at: DateTime<FixedOffset>,
    pub secs: u32,
    pub mood: Option<String>,
    pub energy_target: Option<u8>,
    pub planned: PlannedWork<'p>,
}

/// Plan what the DJ would play from `start` over `span` (at most
/// [`MAX_WORKS`]), given what `ctx` knows: its history, the house's, the
/// feedback and any explicit energy target. `steering` answers the steering
/// at each pick's start (a stored session, or the time-of-day program's
/// mood, which changes through the night); it stands in for `ctx.steer`.
/// `ctx`'s `now` and `local_hour` follow the rehearsal's clock, in
/// `start`'s offset. Nothing is written: the history the set grows is its
/// own. Deterministic for a given `rng` state.
#[must_use]
pub fn rehearse<'p>(
    pool: &'p WorkPool,
    ctx: &PickContext<'_>,
    config: &DjConfig,
    start: DateTime<FixedOffset>,
    span: Span,
    steering: &dyn Fn(DateTime<FixedOffset>) -> Option<Steer>,
    rng: &mut Rng,
) -> Vec<Rehearsed<'p>> {
    let (limit, end) = match span {
        Span::Works(n) => (n.min(MAX_WORKS), None),
        Span::Minutes(m) => (MAX_WORKS, Some(start + TimeDelta::minutes(i64::from(m)))),
    };
    let mut history = ctx.history.to_vec();
    let mut set = Vec::new();
    let mut at = start;
    while set.len() < limit && end.is_none_or(|end| at < end) {
        let steer = steering(at);
        let step = PickContext {
            history: &history,
            now: Some(at.timestamp()),
            local_hour: u8::try_from(at.hour()).ok(),
            steer: steer.as_ref(),
            ..*ctx
        };
        let energy_target = energy_target(pool, &step);
        let mood = step
            .steer
            .filter(|s| s.constraints.is_active(step.now))
            .and_then(|s| s.mood.clone());
        let Some(planned) = pick_next(pool, &step, config, rng) else {
            break;
        };
        let began = at;
        for m in planned.movements {
            history.push(PlayRecord::at(m.track.source_uri.clone(), at.timestamp()));
            let secs = m.track.duration_secs.unwrap_or(UNKNOWN_MOVEMENT_SECS);
            at += TimeDelta::seconds(i64::from(secs));
        }
        let secs = u32::try_from((at - began).num_seconds()).unwrap_or(u32::MAX);
        set.push(Rehearsed {
            at: began,
            secs,
            mood,
            energy_target,
            planned,
        });
    }
    set
}

/// The works of a rehearsal starting in one local hour.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HourSummary {
    /// When the hour's first work starts.
    pub from: DateTime<FixedOffset>,
    pub works: usize,
    /// Shannon entropy, in bits, of the works' artists (a classical work's
    /// composer, a song's lead artist): 0 when one artist plays the hour,
    /// log2(works) when every work has its own.
    pub artist_entropy: f64,
    /// The same over their genres (a work without one counts as one more).
    pub genre_entropy: f64,
    /// Their mean energy, 0-100.
    pub energy: u8,
    /// The energy the DJ aimed for as the hour's first work started.
    pub energy_target: Option<u8>,
}

/// What a rehearsal adds up to.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Summary {
    pub works: usize,
    pub minutes: u32,
    pub distinct_artists: usize,
    /// Works with an explicit movement.
    pub explicit: usize,
    /// Hour by hour, in order (a set past midnight has a new 0 hour).
    pub hours: Vec<HourSummary>,
}

/// Sum up `set`, hour by hour.
#[must_use]
pub fn summarize(set: &[Rehearsed<'_>]) -> Summary {
    let mut hours: Vec<&[Rehearsed<'_>]> = Vec::new();
    let mut from = 0;
    for i in 1..=set.len() {
        let next_hour = set.get(i).is_none_or(|r| {
            r.at.hour() != set[from].at.hour() || r.at - set[from].at >= TimeDelta::hours(1)
        });
        if next_hour {
            hours.push(&set[from..i]);
            from = i;
        }
    }
    let artist = |r: &Rehearsed<'_>| r.planned.work.composer_key().to_owned();
    Summary {
        works: set.len(),
        minutes: set.iter().map(|r| r.secs).sum::<u32>() / 60,
        distinct_artists: set.iter().map(artist).collect::<HashSet<_>>().len(),
        explicit: set
            .iter()
            .filter(|r| r.planned.movements.iter().any(|m| m.explicit))
            .count(),
        hours: hours
            .into_iter()
            .map(|works| {
                let energy: u32 = works
                    .iter()
                    .map(|r| u32::from(r.planned.work.energy()))
                    .sum();
                let count = u32::try_from(works.len()).unwrap_or(u32::MAX);
                HourSummary {
                    from: works[0].at,
                    works: works.len(),
                    artist_entropy: entropy(works.iter().map(artist)),
                    genre_entropy: entropy(
                        works
                            .iter()
                            .map(|r| r.planned.work.genre().unwrap_or_default().to_owned()),
                    ),
                    energy: u8::try_from(energy / count.max(1)).unwrap_or(u8::MAX),
                    energy_target: works[0].energy_target,
                }
            })
            .collect(),
    }
}

/// Shannon entropy, in bits, of how `keys` spread.
fn entropy(keys: impl Iterator<Item = String>) -> f64 {
    let mut counts: HashMap<String, u32> = HashMap::new();
    let mut total = 0_u32;
    for key in keys {
        *counts.entry(key).or_default() += 1;
        total += 1;
    }
    let total = f64::from(total);
    counts
        .values()
        .map(|&n| {
            let p = f64::from(n) / total;
            -p * p.log2()
        })
        .sum::<f64>()
        // One key alone sums to -0.0.
        .abs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dj::{DjConfig, energy_target_for_hour};
    use crate::steer::DjConstraints;
    use crate::test_shelf::{MIDNIGHT, mixed_items, shelf_items, works_of};

    fn at(hour: u32) -> DateTime<FixedOffset> {
        DateTime::from_timestamp(MIDNIGHT + i64::from(hour) * 3600, 0)
            .unwrap()
            .fixed_offset()
    }

    fn keys(set: &[Rehearsed<'_>]) -> Vec<(i64, String)> {
        set.iter()
            .map(|r| (r.at.timestamp(), r.planned.work.work_key.clone()))
            .collect()
    }

    #[test]
    fn a_seed_rehearses_the_same_set_and_the_clock_follows_each_work() {
        let pool = works_of(&shelf_items(2));
        let config = DjConfig::default();
        let run = |seed| {
            rehearse(
                &pool,
                &PickContext::default(),
                &config,
                at(20),
                Span::Works(12),
                &|_| None,
                &mut Rng::new(seed),
            )
        };
        let set = run(7);
        assert_eq!(set.len(), 12);
        assert_eq!(keys(&set), keys(&run(7)), "deterministic for a seed");
        assert_ne!(keys(&set), keys(&run(8)), "another seed, another set");
        assert_eq!(set[0].at, at(20));
        for pair in set.windows(2) {
            let length: u32 = pair[0]
                .planned
                .movements
                .iter()
                .map(|m| m.track.duration_secs.unwrap())
                .sum();
            assert_eq!(pair[0].secs, length);
            assert_eq!(
                pair[1].at,
                pair[0].at + TimeDelta::seconds(i64::from(length))
            );
        }
        // Nothing repeats within the set: each pick saw the earlier ones.
        let distinct: HashSet<&str> = set
            .iter()
            .map(|r| r.planned.work.work_key.as_str())
            .collect();
        assert_eq!(distinct.len(), set.len());
    }

    #[test]
    fn an_hours_span_ends_with_the_last_work_started_in_it() {
        let pool = works_of(&shelf_items(3));
        let set = rehearse(
            &pool,
            &PickContext::default(),
            &DjConfig::default(),
            at(22),
            Span::Minutes(180),
            &|_| None,
            &mut Rng::new(3),
        );
        let last = set.last().unwrap();
        assert!(last.at < at(25), "{}", last.at);
        assert!(
            last.at + TimeDelta::seconds(i64::from(last.secs)) >= at(25),
            "the set fills the three hours"
        );
    }

    #[test]
    fn the_energy_target_follows_the_clock_through_the_night() {
        let pool = works_of(&shelf_items(3));
        let ctx = PickContext::default();
        let set = rehearse(
            &pool,
            &ctx,
            &DjConfig::default(),
            at(21),
            Span::Minutes(9 * 60),
            &|_| None,
            &mut Rng::new(5),
        );
        let summary = summarize(&set);
        assert_eq!(summary.works, set.len());
        let by_hour: Vec<(u32, Option<u8>)> = summary
            .hours
            .iter()
            .map(|h| (h.from.hour(), h.energy_target))
            .collect();
        for (hour, target) in &by_hour {
            assert_eq!(
                *target,
                Some(energy_target_for_hour(u8::try_from(*hour).unwrap()))
            );
        }
        let hours: Vec<u32> = by_hour.iter().map(|(h, _)| *h).collect();
        assert!(hours.starts_with(&[21, 22, 23, 0]), "{hours:?}");
        assert_eq!(
            summary.hours.iter().map(|h| h.works).sum::<usize>(),
            set.len()
        );
    }

    #[test]
    fn a_mood_steers_until_it_lapses_and_the_summary_counts_what_played() {
        let pool = works_of(&mixed_items());
        // Calm until 22:00, then the time of day again.
        let steer = Steer {
            mood: Some("calm".into()),
            constraints: DjConstraints {
                energy_bias: -2,
                expires_at: Some(at(22).timestamp()),
                ..DjConstraints::default()
            },
        };
        let set = rehearse(
            &pool,
            &PickContext::default(),
            &DjConfig::default(),
            at(21),
            Span::Minutes(120),
            &|_| Some(steer.clone()),
            &mut Rng::new(11),
        );
        assert_eq!(set[0].mood.as_deref(), Some("calm"));
        assert!(
            set.iter().all(|r| r.mood.is_some() == (r.at < at(22))),
            "the mood names each pick it steered, and none after it lapsed"
        );
        let summary = summarize(&set);
        let steered = summary.hours.first().unwrap();
        let after = summary.hours.iter().find(|h| h.from >= at(22)).unwrap();
        assert!(
            steered.energy_target < Some(energy_target_for_hour(21)),
            "{steered:?}"
        );
        assert_eq!(
            after.energy_target,
            Some(energy_target_for_hour(
                after.from.hour().try_into().unwrap()
            ))
        );
        assert_eq!(
            summary.explicit,
            set.iter()
                .filter(|r| r.planned.movements.iter().any(|m| m.explicit))
                .count()
        );
        assert!(summary.distinct_artists >= 2, "{summary:?}");
    }

    #[test]
    fn entropy_is_zero_for_one_key_and_one_bit_for_two_even_ones() {
        let of = |keys: &[&str]| entropy(keys.iter().map(|k| (*k).to_owned()));
        assert_eq!(
            of(&["a", "a", "a"]).to_bits(),
            0.0_f64.to_bits(),
            "0, not -0.0"
        );
        assert!((of(&["a", "b", "a", "b"]) - 1.0).abs() < 1e-9);
        assert!((of(&["a", "b", "c", "d"]) - 2.0).abs() < 1e-9);
    }

    #[test]
    fn an_empty_pool_rehearses_nothing() {
        let pool = WorkPool::default();
        let set = rehearse(
            &pool,
            &PickContext::default(),
            &DjConfig::default(),
            at(20),
            Span::Works(5),
            &|_| None,
            &mut Rng::new(1),
        );
        assert!(set.is_empty());
        let summary = summarize(&set);
        assert_eq!((summary.works, summary.minutes), (0, 0));
        assert!(summary.hours.is_empty());
    }
}
