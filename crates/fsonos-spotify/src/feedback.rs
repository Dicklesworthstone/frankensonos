//! Learning from the owner: likes, dislikes, early skips and full listens.
//!
//! A DJ that never learns keeps making the same mistakes. Each
//! [`FeedbackSignal`] nudges a work, its composer, or its performer; the
//! [`FeedbackModel`] decays them (half-life 30 days) into weight multipliers
//! the DJ applies, each clamped to [0.25, 2.0], so taste drifts toward the
//! owner's without manual curation and nothing dominates. Two dislikes of a
//! work within 180 days of each other keep it out for 180 days after the
//! second — nothing is banned for good.
//!
//! Persistence goes through [`FeedbackSource`]; [`StoreFeedback`] reads the
//! store's `feedback` table (rows written with [`FeedbackSignal::to_store`]).
//! Pure: the caller supplies the clock.

use std::collections::HashMap;
use std::ops::Range;

use fsonos_core::store::{Feedback, Store};
use serde::{Deserialize, Serialize};

use crate::SpotifyError;
use crate::classical::{composer_matches, normalize};
use crate::library::split_artists;
use crate::works::Work;

/// Feedback half-life, in seconds (30 days).
pub const HALF_LIFE_SECS: i64 = 30 * 86_400;
/// How long two dislikes keep a work out, and how close together they must
/// be, in seconds (180 days).
pub const EXCLUSION_SECS: i64 = 180 * 86_400;
/// A skip within this many seconds of a track starting is an early skip.
pub const EARLY_SKIP_SECS: i64 = 30;
/// Multipliers stay within [MIN, MAX] (per-mille).
const MIN_PM: u64 = 250;
const MAX_PM: u64 = 2000;
/// Decayed score that doubles (or halves) a weight.
const SCORE_PER_DOUBLING: f64 = 6.0;

/// One kind of feedback, with its strength.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Signal {
    /// An explicit like: +3.
    Like,
    /// An explicit dislike: −3.
    Dislike,
    /// Skipped within 30 s of starting: −1.
    EarlySkip,
    /// Played to the end: +1.
    FullListen,
}

impl Signal {
    #[must_use]
    pub fn value(self) -> i64 {
        match self {
            Self::Like => 3,
            Self::Dislike => -3,
            Self::EarlySkip => -1,
            Self::FullListen => 1,
        }
    }

    #[must_use]
    pub fn from_value(value: i64) -> Option<Self> {
        match value {
            3 => Some(Self::Like),
            -3 => Some(Self::Dislike),
            -1 => Some(Self::EarlySkip),
            1 => Some(Self::FullListen),
            _ => None,
        }
    }
}

/// Feedback about a work, a composer, or a performer (any of the keys may be
/// set: a like on a work usually carries its composer and performer too).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedbackSignal {
    /// Unix seconds.
    pub at: i64,
    /// [`Work::work_key`].
    pub work_key: Option<String>,
    /// The composer's key (normalized name).
    pub composer_key: Option<String>,
    /// A performer, normalized ([`normalize`]).
    pub performer: Option<String>,
    pub signal: Signal,
}

impl FeedbackSignal {
    /// A signal about a whole work, carrying its composer and lead performer.
    #[must_use]
    pub fn about(work: &Work, signal: Signal, at: i64) -> Self {
        Self {
            at,
            work_key: Some(work.work_key.clone()),
            composer_key: Some(work.composer_key().to_owned()),
            performer: performers(work).into_iter().next(),
            signal,
        }
    }

    /// The store row for this signal.
    #[must_use]
    pub fn to_store(&self) -> Feedback {
        Feedback {
            at: self.at,
            work_key: self.work_key.clone(),
            composer_key: self.composer_key.clone(),
            performer: self.performer.clone(),
            signal: self.signal.value(),
        }
    }

    /// A store row as a signal; `None` for a signal value this crate doesn't
    /// know.
    #[must_use]
    pub fn from_store(row: &Feedback) -> Option<Self> {
        Some(Self {
            at: row.at,
            work_key: row.work_key.clone(),
            composer_key: row.composer_key.clone(),
            performer: row.performer.clone(),
            signal: Signal::from_value(row.signal)?,
        })
    }
}

/// Where feedback is kept. The daemon implements it over the store.
pub trait FeedbackSource {
    /// Every signal given within `window` (unix seconds, end exclusive),
    /// oldest first.
    fn signals(&self, window: Range<i64>) -> Result<Vec<FeedbackSignal>, SpotifyError>;
}

impl FeedbackSource for [FeedbackSignal] {
    fn signals(&self, window: Range<i64>) -> Result<Vec<FeedbackSignal>, SpotifyError> {
        Ok(self
            .iter()
            .filter(|s| window.contains(&s.at))
            .cloned()
            .collect())
    }
}

/// A [`Store`]'s feedback table as a [`FeedbackSource`]. Rows with a signal
/// value this crate doesn't know are skipped.
#[derive(Debug, Clone, Copy)]
pub struct StoreFeedback<'s, S: ?Sized>(pub &'s S);

impl<S: Store + ?Sized> FeedbackSource for StoreFeedback<'_, S> {
    fn signals(&self, window: Range<i64>) -> Result<Vec<FeedbackSignal>, SpotifyError> {
        Ok(self
            .0
            .feedback_between(window)?
            .iter()
            .filter_map(FeedbackSignal::from_store)
            .collect())
    }
}

/// Whether a track the owner stopped at `skipped_at` (unix seconds) was
/// skipped early: within 30 s of starting, and before it would have ended.
#[must_use]
pub fn early_skip(started_at: i64, skipped_at: i64, duration_secs: Option<u32>) -> bool {
    let heard = skipped_at - started_at;
    (0..EARLY_SKIP_SECS).contains(&heard) && duration_secs.is_none_or(|d| heard < i64::from(d))
}

/// The implicit signal a track's end gives: an early skip, a full listen
/// (it ended on its own, or 90% of it was heard), or nothing.
#[must_use]
pub fn listen_signal(
    started_at: i64,
    ended_at: i64,
    duration_secs: Option<u32>,
    skipped: bool,
) -> Option<Signal> {
    if skipped && early_skip(started_at, ended_at, duration_secs) {
        return Some(Signal::EarlySkip);
    }
    let heard = ended_at - started_at;
    let most = duration_secs.is_some_and(|d| heard * 10 >= i64::from(d) * 9);
    (!skipped || most).then_some(Signal::FullListen)
}

/// The work's performers: its credited artists other than the composer,
/// normalized.
#[must_use]
pub fn performers(work: &Work) -> Vec<String> {
    let Some(first) = work.movements.first() else {
        return Vec::new();
    };
    split_artists(first.track.artist.as_deref().unwrap_or_default())
        .iter()
        .filter(|artist| !composer_matches(artist, &work.composer))
        .map(|artist| normalize(artist))
        .filter(|artist| !artist.is_empty())
        .collect()
}

/// Feedback decayed to one moment: weight multipliers per work, composer
/// and performer, and the works two dislikes keep out.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FeedbackModel {
    /// The moment it was decayed to (unix seconds).
    at: i64,
    works: HashMap<String, f64>,
    composers: HashMap<String, f64>,
    performers: HashMap<String, f64>,
    /// Work key → until when (unix seconds) it is excluded.
    excluded: HashMap<String, i64>,
}

impl FeedbackModel {
    /// The model at `now` from every signal that can still matter: those of
    /// the last two exclusion windows (older ones have decayed below 1/4000).
    pub fn load<F: FeedbackSource + ?Sized>(source: &F, now: i64) -> Result<Self, SpotifyError> {
        let since = now - 2 * EXCLUSION_SECS;
        Ok(Self::from_signals(&source.signals(since..now + 1)?, now))
    }

    /// The model at `now` from `signals` (later-than-`now` ones ignored).
    #[must_use]
    pub fn from_signals(signals: &[FeedbackSignal], now: i64) -> Self {
        let mut model = Self {
            at: now,
            ..Self::default()
        };
        let mut dislikes: HashMap<&str, Vec<i64>> = HashMap::new();
        for s in signals.iter().filter(|s| s.at <= now) {
            let age = now - s.at;
            #[allow(clippy::cast_precision_loss)] // ages are far below 2^52 s
            let weight = (-(age as f64) / HALF_LIFE_SECS as f64).exp2();
            #[allow(clippy::cast_precision_loss)]
            let score = s.signal.value() as f64 * weight;
            for (map, key) in [
                (&mut model.works, &s.work_key),
                (&mut model.composers, &s.composer_key),
                (&mut model.performers, &s.performer),
            ] {
                if let Some(key) = key {
                    *map.entry(key.clone()).or_default() += score;
                }
            }
            if s.signal == Signal::Dislike
                && let Some(work) = &s.work_key
            {
                dislikes.entry(work.as_str()).or_default().push(s.at);
            }
        }
        for (work, mut at) in dislikes {
            at.sort_unstable();
            let until = at
                .windows(2)
                .filter(|pair| pair[1] - pair[0] < EXCLUSION_SECS)
                .map(|pair| pair[1] + EXCLUSION_SECS)
                .max();
            if let Some(until) = until.filter(|&until| now < until) {
                model.excluded.insert(work.to_owned(), until);
            }
        }
        model
    }

    /// Whether two dislikes keep `work` out (at the model's moment).
    /// Twice-disliked works are out until 180 days after the second dislike.
    #[must_use]
    pub fn excludes(&self, work: &Work) -> bool {
        self.excluded
            .get(&work.work_key)
            .is_some_and(|&until| self.at < until)
    }

    /// The weight multiplier for `work`, per-mille: the product of its work,
    /// composer and performer multipliers, each — and the product —
    /// clamped to [250, 2000].
    #[must_use]
    pub fn multiplier_pm(&self, work: &Work) -> u64 {
        let mut pm = 1000u64;
        let mut apply = |score: Option<&f64>| {
            if let Some(&score) = score {
                pm = pm * score_pm(score) / 1000;
            }
        };
        apply(self.works.get(&work.work_key));
        apply(self.composers.get(work.composer_key()));
        for performer in performers(work) {
            apply(self.performers.get(&performer));
        }
        pm.clamp(MIN_PM, MAX_PM)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.works.is_empty() && self.composers.is_empty() && self.performers.is_empty()
    }
}

/// A decayed score as a multiplier: ×2 per +6, ×½ per −6, clamped.
fn score_pm(score: f64) -> u64 {
    let factor = (score / SCORE_PER_DOUBLING).exp2() * 1000.0;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // clamped first
    let pm = factor.clamp(250.0, 2000.0).round() as u64;
    pm
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dj::{DjConfig, Factor, PlannedWork, WorkPool};
    use crate::steer::{DjConstraints, Steer};
    use crate::test_shelf::{MIDNIGHT, shelf_items, simulate_with, transcript, works_of};
    use fsonos_core::store::{FeedbackKey, MemStore, SqliteStore, Store};

    const DAY: i64 = 86_400;

    fn signal(
        work: Option<&str>,
        composer: Option<&str>,
        signal: Signal,
        at: i64,
    ) -> FeedbackSignal {
        FeedbackSignal {
            at,
            work_key: work.map(str::to_owned),
            composer_key: composer.map(str::to_owned),
            performer: None,
            signal,
        }
    }

    fn by<'p>(pool: &'p WorkPool, surname: &str) -> Vec<&'p Work> {
        pool.works()
            .iter()
            .filter(|w| w.composer.ends_with(surname))
            .collect()
    }

    #[test]
    fn signals_decay_with_a_thirty_day_half_life() {
        let pool = works_of(&shelf_items(1));
        let work = by(&pool, "Mozart")[0];
        let key = Some(work.work_key.as_str());
        let like = [signal(key, None, Signal::Like, MIDNIGHT)];
        let at = |days: i64| FeedbackModel::from_signals(&like, MIDNIGHT + days * DAY);
        // +3 doubles a weight at 6: 2^(3/6), then half the score per 30 days.
        assert_eq!(at(0).multiplier_pm(work), 1414);
        assert_eq!(at(30).multiplier_pm(work), 1189);
        assert_eq!(at(60).multiplier_pm(work), 1091);
        assert_eq!(at(365).multiplier_pm(work), 1000);
        assert_eq!(
            FeedbackModel::from_signals(&like, MIDNIGHT - 1).multiplier_pm(work),
            1000,
            "a signal from the future is ignored"
        );

        let one = |s: Signal| {
            FeedbackModel::from_signals(&[signal(key, None, s, MIDNIGHT)], MIDNIGHT)
                .multiplier_pm(work)
        };
        assert_eq!(one(Signal::Dislike), 707);
        assert_eq!(one(Signal::EarlySkip), 891);
        assert_eq!(one(Signal::FullListen), 1122);

        // Scores add: a like a month ago and a dislike today net −1.5.
        let mixed = [
            signal(key, None, Signal::Like, MIDNIGHT - 30 * DAY),
            signal(key, None, Signal::Dislike, MIDNIGHT),
        ];
        assert_eq!(
            FeedbackModel::from_signals(&mixed, MIDNIGHT).multiplier_pm(work),
            841
        );
        let other = by(&pool, "Mozart")[1];
        assert_eq!(at(0).multiplier_pm(other), 1000, "only that work");
        assert!(FeedbackModel::default().is_empty() && !at(0).is_empty());
    }

    #[test]
    fn multipliers_are_clamped_to_a_quarter_and_double() {
        let pool = works_of(&shelf_items(1));
        let work = by(&pool, "Brahms")[0];
        assert_eq!(performers(work), ["test ensemble"]);
        let five = |s: Signal| -> Vec<FeedbackSignal> {
            (0..5)
                .map(|_| FeedbackSignal::about(work, s, MIDNIGHT))
                .collect()
        };
        // Work, composer and performer each at the cap: the product is too.
        let loved = FeedbackModel::from_signals(&five(Signal::Like), MIDNIGHT);
        assert_eq!(loved.multiplier_pm(work), 2000);
        let hated = FeedbackModel::from_signals(&five(Signal::Dislike), MIDNIGHT);
        assert_eq!(hated.multiplier_pm(work), 250);

        // Each factor is clamped before multiplying: +15 on the work counts
        // as ×2, so a −3 on the composer brings it to ×1.414 (not ×4 → ×2).
        let key = Some(work.work_key.as_str());
        let mut signals: Vec<FeedbackSignal> = (0..5)
            .map(|_| signal(key, None, Signal::Like, MIDNIGHT))
            .collect();
        signals.push(signal(
            None,
            Some(work.composer_key()),
            Signal::Dislike,
            MIDNIGHT,
        ));
        let model = FeedbackModel::from_signals(&signals, MIDNIGHT);
        assert_eq!(model.multiplier_pm(work), 1414);
        // The composer's other works carry its multiplier alone, and a
        // shared performer's feedback reaches every work they play.
        assert_eq!(model.multiplier_pm(by(&pool, "Brahms")[1]), 707);
        assert_eq!(model.multiplier_pm(by(&pool, "Haydn")[0]), 1000);
        let ensemble = [FeedbackSignal {
            performer: Some("test ensemble".into()),
            ..signal(None, None, Signal::Like, MIDNIGHT)
        }];
        let model = FeedbackModel::from_signals(&ensemble, MIDNIGHT);
        assert_eq!(model.multiplier_pm(by(&pool, "Haydn")[0]), 1414);
    }

    #[test]
    fn two_dislikes_exclude_a_work_for_180_days() {
        let pool = works_of(&shelf_items(1));
        let work = by(&pool, "Vivaldi")[0];
        let key = Some(work.work_key.as_str());
        let dislike = |day: i64| signal(key, None, Signal::Dislike, MIDNIGHT + day * DAY);
        let excluded = |signals: &[FeedbackSignal], day: i64| {
            FeedbackModel::from_signals(signals, MIDNIGHT + day * DAY).excludes(work)
        };

        let twice = [dislike(0), dislike(10)];
        assert!(!excluded(&twice[..1], 1), "one dislike only weighs it down");
        assert!(!excluded(&twice, 9), "not before the second");
        assert!(excluded(&twice, 10));
        assert!(excluded(&twice, 189));
        let model = FeedbackModel::from_signals(&twice, MIDNIGHT + 190 * DAY - 1);
        assert!(model.excludes(work), "until 180 days after the second");
        assert!(!excluded(&twice, 190), "then it may play again");
        assert!(!excluded(&[dislike(0), dislike(180)], 180), "too far apart");
        assert!(excluded(&[dislike(0), dislike(179)], 300));
        // A third dislike renews the window.
        assert!(excluded(&[dislike(0), dislike(10), dislike(100)], 279));
        assert!(!excluded(&[dislike(0), dislike(10), dislike(100)], 280));
        // Disliking a composer weighs its works down but excludes none.
        let composer = signal(None, Some(work.composer_key()), Signal::Dislike, MIDNIGHT);
        assert!(!excluded(&[composer.clone(), composer], 1));
        let model = FeedbackModel::from_signals(&twice, MIDNIGHT + 100 * DAY);
        assert!(!model.excludes(by(&pool, "Vivaldi")[1]), "only that work");

        // Loading reaches far enough back to see both dislikes of a pair.
        let source: &[FeedbackSignal] = &[dislike(0), dislike(170)];
        let model = FeedbackModel::load(source, MIDNIGHT + 340 * DAY).unwrap();
        assert!(model.excludes(work));
        assert!(
            !FeedbackModel::load(source, MIDNIGHT + 350 * DAY)
                .unwrap()
                .excludes(work)
        );
    }

    #[test]
    fn early_skips_and_full_listens_are_detected() {
        assert!(early_skip(MIDNIGHT, MIDNIGHT + 10, Some(240)));
        assert!(early_skip(MIDNIGHT, MIDNIGHT + 29, None));
        assert!(early_skip(MIDNIGHT, MIDNIGHT, Some(240)));
        assert!(
            !early_skip(MIDNIGHT, MIDNIGHT + 30, Some(240)),
            "30 s heard"
        );
        assert!(
            !early_skip(MIDNIGHT, MIDNIGHT + 20, Some(15)),
            "it had ended"
        );
        assert!(!early_skip(MIDNIGHT, MIDNIGHT - 5, Some(240)), "clock skew");

        let listen = |heard: i64, skipped: bool| {
            listen_signal(MIDNIGHT, MIDNIGHT + heard, Some(240), skipped)
        };
        assert_eq!(listen(12, true), Some(Signal::EarlySkip));
        assert_eq!(listen(240, false), Some(Signal::FullListen));
        assert_eq!(listen(216, true), Some(Signal::FullListen), "90% heard");
        assert_eq!(listen(215, true), None);
        assert_eq!(listen(100, true), None);
        assert_eq!(
            listen_signal(MIDNIGHT, MIDNIGHT + 400, None, true),
            None,
            "a late skip of a track of unknown length says nothing"
        );
    }

    #[test]
    fn signals_round_trip_through_both_stores() {
        let pool = works_of(&shelf_items(1));
        let work = by(&pool, "Chopin")[0];
        let given = [
            FeedbackSignal::about(work, Signal::EarlySkip, MIDNIGHT),
            FeedbackSignal::about(work, Signal::Like, MIDNIGHT + 60),
        ];
        assert_eq!(given[0].composer_key.as_deref(), Some(work.composer_key()));
        assert_eq!(given[0].performer.as_deref(), Some("test ensemble"));
        let stores: [Box<dyn Store>; 2] = [
            Box::new(MemStore::default()),
            Box::new(SqliteStore::open_in_memory().unwrap()),
        ];
        for mut store in stores {
            for s in &given {
                store.record_feedback(&s.to_store()).unwrap();
            }
            let rows = store
                .feedback(FeedbackKey::Work(&work.work_key), MIDNIGHT..MIDNIGHT + DAY)
                .unwrap();
            let back: Vec<FeedbackSignal> =
                rows.iter().filter_map(FeedbackSignal::from_store).collect();
            assert_eq!(back, given);

            // The model loads straight from the store. Rows with a signal
            // this crate doesn't know are skipped, and so are rows older
            // than the model reads.
            for at in [MIDNIGHT + 120, MIDNIGHT + 180] {
                let dislike = FeedbackSignal::about(work, Signal::Dislike, at);
                store.record_feedback(&dislike.to_store()).unwrap();
            }
            let unknown = Feedback {
                signal: 7,
                ..given[0].to_store()
            };
            store.record_feedback(&unknown).unwrap();
            let key = Some(work.work_key.as_str());
            let ancient = signal(key, None, Signal::Like, MIDNIGHT - 400 * DAY);
            store.record_feedback(&ancient.to_store()).unwrap();
            let source = StoreFeedback(&*store);
            let all = source
                .signals(MIDNIGHT - 400 * DAY..MIDNIGHT + DAY)
                .unwrap();
            assert_eq!(all.len(), 5, "{all:?}");
            assert_eq!(all[0], ancient, "oldest first");
            let model = FeedbackModel::load(&source, MIDNIGHT + DAY).unwrap();
            assert!(model.excludes(work), "disliked twice");
            // −1 +3 −3 −3 on the work, its composer and its performer, a day
            // old: ×0.637 three times.
            assert_eq!(model.multiplier_pm(work), 257);
            assert_eq!(
                model,
                FeedbackModel::from_signals(&all[1..], MIDNIGHT + DAY),
                "a 400-day-old like is out of reach"
            );
        }
        for s in [
            Signal::Like,
            Signal::Dislike,
            Signal::EarlySkip,
            Signal::FullListen,
        ] {
            assert_eq!(Signal::from_value(s.value()), Some(s));
        }
        let odd = Feedback {
            signal: 7,
            ..given[0].to_store()
        };
        assert_eq!(FeedbackSignal::from_store(&odd), None);
        assert_eq!(
            serde_json::to_string(&Signal::EarlySkip).unwrap(),
            "\"early_skip\""
        );
    }

    fn count(picks: &[PlannedWork<'_>], surname: &str) -> usize {
        picks
            .iter()
            .filter(|p| p.work.composer.ends_with(surname))
            .count()
    }

    #[test]
    fn seeded_plans_shift_toward_liked_composers() {
        let pool = works_of(&shelf_items(3));
        let config = DjConfig::default();
        let mozart = by(&pool, "Mozart")[0].composer_key().to_owned();
        let twice = |s: Signal| -> Vec<FeedbackSignal> {
            (0..2)
                .map(|_| signal(None, Some(&mozart), s, MIDNIGHT - DAY))
                .collect()
        };
        let liked = FeedbackModel::from_signals(&twice(Signal::Like), MIDNIGHT);
        let disliked = FeedbackModel::from_signals(&twice(Signal::Dislike), MIDNIGHT);
        let (mut base, mut more, mut less) = (0, 0, 0);
        let mut log = String::new();
        for seed in 0..6 {
            let plan = |model| simulate_with(&pool, &config, seed, 60, None, None, model);
            let (b, m, l) = (plan(None), plan(Some(&liked)), plan(Some(&disliked)));
            base += count(&b, "Mozart");
            more += count(&m, "Mozart");
            less += count(&l, "Mozart");
            for p in &m {
                let is_mozart = p.work.composer.ends_with("Mozart");
                assert_eq!(
                    p.reason.has(Factor::Feedback),
                    is_mozart,
                    "{}",
                    transcript(seed, &pool, &m)
                );
                assert_eq!(
                    p.reason.summary.contains("favored by your feedback"),
                    is_mozart
                );
            }
            for p in l.iter().filter(|p| p.work.composer.ends_with("Mozart")) {
                assert!(p.reason.summary.contains("played less after your feedback"));
            }
            if seed == 0 {
                log = format!(
                    "{}\n{}",
                    transcript(seed, &pool, &b),
                    transcript(seed, &pool, &m)
                );
            }
        }
        // Composer spacing still holds (a composer is rarely back within four
        // works), so doubling the weight shifts the share by about a third.
        assert!(
            more * 4 >= base * 5,
            "liked Mozart: {more} picks vs {base} without feedback\n{log}"
        );
        assert!(
            less * 3 <= base * 2,
            "disliked Mozart: {less} picks vs {base} without feedback\n{log}"
        );
    }

    #[test]
    fn twice_disliked_works_are_never_planned() {
        let pool = works_of(&shelf_items(1));
        let config = DjConfig::default();
        let chopin = by(&pool, "Chopin");
        let signals: Vec<FeedbackSignal> = chopin
            .iter()
            .flat_map(|w| {
                [20, 10].map(|days| {
                    signal(
                        Some(w.work_key.as_str()),
                        None,
                        Signal::Dislike,
                        MIDNIGHT - days * DAY,
                    )
                })
            })
            .collect();
        let model = FeedbackModel::from_signals(&signals, MIDNIGHT);
        assert!(chopin.iter().all(|w| model.excludes(w)));
        for seed in 0..3 {
            let picks = simulate_with(&pool, &config, seed, 90, None, None, Some(&model));
            assert_eq!(
                count(&picks, "Chopin"),
                0,
                "{}",
                transcript(seed, &pool, &picks)
            );
        }
        // Asked for Chopin and nothing else, the exclusion yields rather
        // than leave nothing to play.
        let steer = Steer {
            mood: None,
            constraints: DjConstraints {
                include_composers: vec!["Chopin".into()],
                ..DjConstraints::default()
            },
        };
        let picks = simulate_with(&pool, &config, 7, 3, None, Some(&steer), Some(&model));
        assert_eq!(
            count(&picks, "Chopin"),
            3,
            "{}",
            transcript(7, &pool, &picks)
        );
        assert!(
            picks
                .iter()
                .all(|p| p.reason.summary.contains("played less after your feedback"))
        );
    }
}
