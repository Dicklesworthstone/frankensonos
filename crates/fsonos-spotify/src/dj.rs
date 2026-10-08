//! The DJ: pure selection over whole works, in whatever genres the owner's
//! library holds.
//!
//! The unit of selection is the [`Work`] ([`crate::works`]): a classical
//! piece's movements, played together and in order, or a song on its own. A
//! parsed work is never split; a title that doesn't parse is a one-track
//! work. A song's "composer" is its lead artist, so the composer spacing and
//! balance below spread a pop or jazz set across artists. Given the
//! [`WorkPool`] and
//! the play history — per-track rows, where a work counts as played when any
//! of its movements plays — pick the next work for pleasant variety:
//!
//! * **Anti-repeat.** A work never recurs within
//!   [`DjConfig::work_cooldown_plays`] work plays or
//!   [`DjConfig::work_cooldown_secs`] seconds, and works heard a while ago stay
//!   de-weighted until well clear of that. If the cooldown leaves too few works
//!   (a small library, a long session), the DJ rotates through the
//!   least-recently-played slice instead of repeating.
//! * **Spread.** The same composer or album is strongly de-weighted for a few
//!   works and recovers quadratically; same-period runs are damped and periods
//!   missing from the last few works are favored; prolific composers are
//!   damped (weight ∝ 1/√works) so a shelf of Bach cantatas doesn't drown out
//!   three Fauré pieces.
//! * **Energy.** A work's energy is its first movement's. Prefer works near
//!   the time-of-day target and avoid jarring jumps from one work to the
//!   next (a finale's Presto into a quiet opening is the concert norm, so the
//!   step is measured between the works' characters, not their edges).
//! * **Shape.** Works longer than [`DjConfig::max_work_minutes`] (full operas,
//!   Passions) are left out unless allowed; works missing movements still play
//!   — what the library has, in order — at reduced weight; works the owner
//!   liked are favored.
//! * **Taste.** With a [`FeedbackModel`] ([`crate::feedback`]), the owner's
//!   decayed likes, dislikes, skips and full listens scale each work's weight
//!   (×0.25 – ×2), and twice-disliked works sit out while any other work
//!   qualifies.
//! * **Preferences.** Under the owner's standing preferences
//!   ([`WorkPool::with_preferences`], [`crate::prefs`]), banned and avoided
//!   works leave the pool before steering (a steer asking for one by name
//!   brings it back), explicit ones stay out unless allowed, favored works
//!   weigh ×2 and pinned ones ×3, and feedback can't push those below
//!   neutral. A preferred energy replaces the time-of-day curve.
//!
//! Every pick carries a [`PickReason`]. Weights are integer per-mille factors
//! and every iteration runs in pool or history order, so a seeded [`Rng`]
//! reproduces a set exactly anywhere.

use std::cmp::Reverse;
use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::classical::{CandidatePool, ClassicalTrack, Period};
use crate::feedback::FeedbackModel;
use crate::prefs::{Preferences, Verdict};
use crate::steer::{Moods, Relaxation, Steer, admit_among, haystack, names};
use crate::works::{Completeness, Work, group_works};

/// A seedable, dependency-free pseudo-random generator (xorshift64*, seeded
/// through splitmix64 so nearby seeds diverge immediately). Keeping randomness
/// in-crate avoids pulling `rand` and makes DJ picks reproducible.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    #[must_use]
    pub fn new(seed: u64) -> Self {
        let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        // xorshift's one forbidden state is zero.
        Self(if z == 0 { 0x9E37_79B9_7F4A_7C15 } else { z })
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform value in `0..n` (n > 0), without modulo bias.
    pub fn below_u64(&mut self, n: u64) -> u64 {
        assert!(n > 0, "Rng::below_u64(0)");
        let zone = u64::MAX - u64::MAX % n;
        loop {
            let x = self.next_u64();
            if x < zone {
                return x % n;
            }
        }
    }

    /// Uniform index in `0..n` (n > 0).
    pub fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.below_u64(as_u64(n))).expect("value below a usize fits a usize")
    }
}

fn as_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// One entry of play history (the store's `play_history` row): a track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayRecord {
    pub source_uri: String,
    /// Unix seconds, when known.
    pub played_at: Option<i64>,
}

impl PlayRecord {
    #[must_use]
    pub fn new(source_uri: impl Into<String>) -> Self {
        Self {
            source_uri: source_uri.into(),
            played_at: None,
        }
    }

    #[must_use]
    pub fn at(source_uri: impl Into<String>, played_at: i64) -> Self {
        Self {
            source_uri: source_uri.into(),
            played_at: Some(played_at),
        }
    }
}

/// Tuning for [`pick_next`] and [`plan`]. Plays and spacings count *work*
/// plays: a work's movements heard back to back count once. The track-based
/// cooldown this replaced (150 track plays) converts at the ~3.7 movements per
/// work of a typical classical library to 40 work plays; the composer and
/// album spacings (8 tracks each) to 4 and 3 works.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DjConfig {
    /// Hard: a work can't recur within this many work plays…
    pub work_cooldown_plays: usize,
    /// …nor within this many seconds (when history carries timestamps).
    pub work_cooldown_secs: i64,
    /// Soft: work plays over which a composer recovers from being heard.
    pub composer_spacing: usize,
    /// Soft: work plays over which an album (≈ the same performers) recovers.
    pub album_spacing: usize,
    /// How many recent work plays count when favoring absent periods.
    pub period_memory: usize,
    /// How many history rows (tracks) to scan; older plays are ignored.
    pub history_horizon: usize,
    /// Weight for works with a movement the owner individually liked.
    pub liked_boost_pm: u64,
    /// Weight for works missing movements (Partial or Unknown).
    pub incomplete_pm: u64,
    /// Works longer than this are left out unless `allow_long_works`.
    pub max_work_minutes: u32,
    pub allow_long_works: bool,
    /// Steering filters relax (keywords, then periods, then length) while
    /// fewer works than this pass them.
    pub min_steered_works: usize,
    /// Complete partly-held works from their albums' track lists
    /// (`crate::expand`); a work that can't be completed still plays what
    /// the library has, at `incomplete_pm`.
    pub expand_partial_works: bool,
}

impl Default for DjConfig {
    fn default() -> Self {
        Self {
            work_cooldown_plays: 40,
            work_cooldown_secs: 24 * 60 * 60,
            composer_spacing: 4,
            album_spacing: 3,
            period_memory: 4,
            history_horizon: 2000,
            liked_boost_pm: 1300,
            incomplete_pm: 600,
            max_work_minutes: 75,
            allow_long_works: false,
            min_steered_works: 5,
            expand_partial_works: true,
        }
    }
}

/// What the DJ knows about "now" when picking.
#[derive(Debug, Clone, Copy, Default)]
pub struct PickContext<'h> {
    /// Play history, one row per track, most recent last.
    pub history: &'h [PlayRecord],
    /// Current Unix time (enables the time-based cooldown).
    pub now: Option<i64>,
    /// Local hour 0–23 (enables the time-of-day energy target).
    pub local_hour: Option<u8>,
    /// Explicit energy target 0–100; overrides the time of day.
    pub energy_target: Option<u8>,
    /// A mood or constraints steering the pick ([`crate::steer`]); ignored
    /// once its constraints have lapsed.
    pub steer: Option<&'h Steer>,
    /// The owner's feedback, decayed to when it was loaded
    /// ([`crate::feedback`]); reload it now and then (daily is plenty).
    pub feedback: Option<&'h FeedbackModel>,
}

/// The energy the DJ aims for at a given local hour: calm through the night,
/// building through the morning, liveliest mid-morning, winding down after
/// dinner.
#[must_use]
pub fn energy_target_for_hour(hour: u8) -> u8 {
    match hour % 24 {
        0..=4 => 20,
        5..=6 => 30,
        7..=8 => 45,
        9..=11 => 60,
        12..=13 => 55,
        14..=16 => 58,
        17..=18 => 50,
        19..=20 => 42,
        21 => 32,
        _ => 25,
    }
}

/// A weight factor that moved a work's odds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Factor {
    /// 1/√(the composer's works in the pool).
    ComposerBalance,
    /// The composer was heard a few works ago.
    ComposerSpacing,
    /// The album was heard a few works ago.
    AlbumSpacing,
    /// The work was heard before and is still recovering.
    Recency,
    /// Its period is on a run.
    PeriodRun,
    /// Its period is missing from the last few works.
    PeriodAbsent,
    /// Closeness to the energy target.
    EnergyFit,
    /// The step from the previous work's energy.
    EnergyJump,
    /// The owner liked one of its movements.
    Liked,
    /// Movements are missing from the library.
    Incomplete,
    /// Chosen from the least-recently-played slice (cooldown exhausted).
    Rotation,
    /// Longer than the long-work limit, allowed this time.
    LongWork,
    /// The owner's feedback on the work, its composer or performer.
    Feedback,
    /// The owner's preferences favor (2000) or pin (3000) it.
    Preference,
    /// Every work left is avoided in the owner's preferences, so the avoids
    /// relaxed (a flag, recorded at 1000).
    Avoided,
}

/// Why a work was chosen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PickReason {
    /// Every non-neutral factor in evaluation order, per-mille (1000 =
    /// neutral). `Rotation` and `LongWork` are flags, recorded at 1000.
    pub factors: Vec<(Factor, i32)>,
    /// Steering filters dropped because too few works passed them, in the
    /// order they relaxed.
    pub relaxed: Vec<Relaxation>,
    /// One readable line, e.g. "Brahms not heard in 4 days; balancing toward
    /// late-Romantic; gentle evening target".
    pub summary: String,
}

impl PickReason {
    #[must_use]
    pub fn has(&self, factor: Factor) -> bool {
        self.factors.iter().any(|&(f, _)| f == factor)
    }
}

/// A planned work: all its movements, in playing order, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedWork<'p> {
    pub work: &'p Work,
    /// `work.movements`: the queue takes them all, in this order.
    pub movements: &'p [ClassicalTrack],
    pub reason: PickReason,
}

/// The works the DJ chooses from, indexed for picking.
#[derive(Debug, Clone, Default)]
pub struct WorkPool {
    works: Vec<Work>,
    /// Track URI → (work, movement).
    by_track: HashMap<String, (usize, usize)>,
    composer_works: HashMap<String, usize>,
    /// Per work, the normalized text steering keywords match against.
    haystacks: Vec<String>,
    /// The owner's standing preferences ([`crate::prefs`]) and, per work,
    /// how they treat it.
    preferences: Preferences,
    verdicts: Vec<Verdict>,
}

impl WorkPool {
    #[must_use]
    pub fn new(pool: &CandidatePool) -> Self {
        Self::from_works(group_works(pool.tracks()))
    }

    #[must_use]
    pub fn from_works(works: Vec<Work>) -> Self {
        let mut by_track = HashMap::new();
        let mut composer_works: HashMap<String, usize> = HashMap::new();
        for (w, work) in works.iter().enumerate() {
            *composer_works
                .entry(work.composer_key().to_owned())
                .or_default() += 1;
            for (m, movement) in work.movements.iter().enumerate() {
                by_track.insert(movement.track.source_uri.clone(), (w, m));
            }
        }
        let haystacks = works.iter().map(haystack).collect();
        let verdicts = works
            .iter()
            .map(|work| Verdict {
                explicit: work.movements.iter().any(|m| m.explicit),
                ..Verdict::default()
            })
            .collect();
        Self {
            works,
            by_track,
            composer_works,
            haystacks,
            preferences: Preferences::default(),
            verdicts,
        }
    }

    /// The pool under the owner's preferences; `moods` resolves the moods
    /// they name (the same moods steering uses).
    #[must_use]
    pub fn with_preferences(mut self, preferences: Preferences, moods: &Moods) -> Self {
        self.verdicts = self
            .works
            .iter()
            .zip(&self.haystacks)
            .map(|(work, hay)| preferences.verdict(work, hay, moods))
            .collect();
        self.preferences = preferences;
        self
    }

    #[must_use]
    pub fn preferences(&self) -> &Preferences {
        &self.preferences
    }

    /// How the preferences treat the work at `index` in [`Self::works`].
    #[must_use]
    pub fn verdict(&self, index: usize) -> Verdict {
        self.verdicts[index]
    }

    /// The works the preferences let play, before steering: no explicit
    /// ones unless allowed, nothing banned or avoided, except what an active
    /// steer asks for by name. Avoids relax (the flag) only when they would
    /// leave nothing.
    fn allowed(&self, steer: Option<&Steer>) -> (Vec<usize>, bool) {
        let named = |w: usize| steer.is_some_and(|s| names(&self.works[w], &s.constraints));
        let playable: Vec<usize> = (0..self.len())
            .filter(|&w| {
                let v = self.verdicts[w];
                (self.preferences.explicit || !v.explicit) && (!v.banned || named(w))
            })
            .collect();
        let kept: Vec<usize> = playable
            .iter()
            .copied()
            .filter(|&w| !self.verdicts[w].avoids() || named(w))
            .collect();
        if kept.is_empty() && !playable.is_empty() {
            (playable, true)
        } else {
            (kept, false)
        }
    }

    #[must_use]
    pub fn works(&self) -> &[Work] {
        &self.works
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.works.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.works.is_empty()
    }

    /// The work a track belongs to.
    #[must_use]
    pub fn work_of(&self, source_uri: &str) -> Option<&Work> {
        self.by_track.get(source_uri).map(|&(w, _)| &self.works[w])
    }
}

/// Pick the next work, or `None` for an empty pool. Deterministic for a
/// given pool, context, config and RNG state.
#[must_use]
pub fn pick_next<'p>(
    pool: &'p WorkPool,
    ctx: &PickContext<'_>,
    config: &DjConfig,
    rng: &mut Rng,
) -> Option<PlannedWork<'p>> {
    if pool.is_empty() {
        return None;
    }
    let recency = Recency::scan(pool, ctx.history, config);
    let steer = ctx.steer.filter(|s| s.constraints.is_active(ctx.now));
    let (allowed, avoids_relaxed) = pool.allowed(steer);
    let (admitted, relaxed) = match steer {
        Some(s) => admit_among(
            &pool.works,
            &pool.haystacks,
            &allowed,
            &s.constraints,
            config.min_steered_works,
        ),
        None => (allowed, Vec::new()),
    };
    if admitted.is_empty() {
        // Everything is banned (or explicit, and that isn't allowed).
        return None;
    }
    let allow_long =
        config.allow_long_works || steer.is_some_and(|s| s.constraints.allow_long_works);
    let base_target = ctx
        .energy_target
        .or(pool.preferences.energy)
        .or_else(|| ctx.local_hour.map(energy_target_for_hour));
    let target = steer.map_or(base_target, |s| s.constraints.biased_target(base_target));
    let eligible = eligible(pool, admitted, allow_long, &recency, ctx, config);
    let weights: Vec<u64> = eligible
        .works
        .iter()
        .map(|&w| weigh(pool, w, &recency, target, config, ctx.feedback, None))
        .collect();
    let chosen = eligible.works[draw(&weights, rng)];

    let mut factors = Vec::new();
    weigh(
        pool,
        chosen,
        &recency,
        target,
        config,
        ctx.feedback,
        Some(&mut factors),
    );
    let work = &pool.works[chosen];
    if eligible.rotation {
        factors.push((Factor::Rotation, 1000));
    }
    if avoids_relaxed {
        factors.push((Factor::Avoided, 1000));
    }
    if is_long(work, config) {
        factors.push((Factor::LongWork, 1000));
    }
    let summary = summarize(
        work,
        &recency,
        ctx,
        Target {
            energy: target,
            preferred: ctx.energy_target.is_none() && pool.preferences.energy.is_some(),
        },
        steer,
        &factors,
        &relaxed,
    );
    Some(PlannedWork {
        work,
        movements: &work.movements,
        reason: PickReason {
            factors,
            relaxed,
            summary,
        },
    })
}

/// Plan `count` works ahead (to keep a queue fed); each pick sees the earlier
/// ones as just played.
#[must_use]
pub fn plan<'p>(
    pool: &'p WorkPool,
    ctx: &PickContext<'_>,
    config: &DjConfig,
    count: usize,
    rng: &mut Rng,
) -> Vec<PlannedWork<'p>> {
    let mut history = ctx.history.to_vec();
    let mut planned = Vec::with_capacity(count);
    for _ in 0..count {
        let step = PickContext {
            history: &history,
            ..*ctx
        };
        let Some(next) = pick_next(pool, &step, config, rng) else {
            break;
        };
        history.extend(next.movements.iter().map(|m| PlayRecord {
            source_uri: m.track.source_uri.clone(),
            played_at: ctx.now,
        }));
        planned.push(next);
    }
    planned
}

/// What the recent history says about each work and grouping key, counted in
/// work plays (0 = the work heard last).
#[derive(Default)]
struct Recency<'p> {
    work_ago: HashMap<usize, usize>,
    work_at: HashMap<usize, i64>,
    composer_ago: HashMap<&'p str, usize>,
    composer_at: HashMap<&'p str, i64>,
    album_ago: HashMap<&'p str, usize>,
    /// Period of the latest work plays and how many in a row share it.
    period_run: Option<(Period, usize)>,
    /// Periods of the last `period_memory` work plays.
    recent_periods: Vec<Period>,
    /// Energy of the previous work, if it was a pool work.
    last_energy: Option<u8>,
}

impl<'p> Recency<'p> {
    fn scan(pool: &'p WorkPool, history: &[PlayRecord], config: &DjConfig) -> Self {
        let mut seen = Self::default();
        let mut ago = 0;
        // The work of the play being counted (`Some(None)`: a non-pool track).
        let mut current: Option<Option<usize>> = None;
        let mut run_open = true;
        let latest_first = history.iter().rev().take(config.history_horizon);
        for (row, play) in latest_first.enumerate() {
            let hit = pool.by_track.get(play.source_uri.as_str()).copied();
            let work = hit.map(|(w, _)| w);
            // A work's movements heard back to back are one play; any other
            // row (including a track outside the pool) is a play of its own.
            let new_play = match current {
                None => true,
                Some(prev) => work.is_none() || work != prev,
            };
            if new_play && current.is_some() {
                ago += 1;
            }
            current = Some(work);
            if row == 0 {
                seen.last_energy = work.map(|w| pool.works[w].energy());
            }
            let Some(w) = work else {
                continue;
            };
            let entry = &pool.works[w];
            let composer = entry.composer_key();
            seen.work_ago.entry(w).or_insert(ago);
            seen.composer_ago.entry(composer).or_insert(ago);
            if !entry.album_key.is_empty() {
                seen.album_ago
                    .entry(entry.album_key.as_str())
                    .or_insert(ago);
            }
            if let Some(at) = play.played_at {
                seen.work_at.entry(w).or_insert(at);
                seen.composer_at.entry(composer).or_insert(at);
            }
            if !new_play {
                continue;
            }
            if seen.recent_periods.len() < config.period_memory {
                seen.recent_periods.push(entry.period);
            }
            if run_open {
                seen.period_run = match seen.period_run {
                    None => Some((entry.period, 1)),
                    Some((period, run)) if period == entry.period => Some((period, run + 1)),
                    other => {
                        run_open = false;
                        other
                    }
                };
            }
        }
        seen
    }
}

struct Eligible {
    works: Vec<usize>,
    /// Chosen from the stalest slice because the cooldown left too few.
    rotation: bool,
}

fn is_long(work: &Work, config: &DjConfig) -> bool {
    u64::from(work.total_secs) > u64::from(config.max_work_minutes) * 60
}

/// Works eligible for this pick, among those the steering `admitted`: the
/// ones within the length limit (all of them if none are, so a library of
/// operas still plays), out of cooldown — or, when that leaves fewer than a
/// tenth of them, the least-recently-played quarter, so a small pool rotates
/// rather than repeats.
fn eligible(
    pool: &WorkPool,
    admitted: Vec<usize>,
    allow_long: bool,
    recency: &Recency<'_>,
    ctx: &PickContext<'_>,
    config: &DjConfig,
) -> Eligible {
    let mut allowed: Vec<usize> = admitted
        .iter()
        .copied()
        .filter(|&w| allow_long || !is_long(&pool.works[w], config))
        .collect();
    if allowed.is_empty() {
        allowed = admitted;
    }
    // Twice-disliked works sit out — unless the owner's preferences favor
    // them, or that would leave nothing.
    if let Some(model) = ctx.feedback {
        let kept: Vec<usize> = allowed
            .iter()
            .copied()
            .filter(|&w| pool.verdicts[w].shields_feedback() || !model.excludes(&pool.works[w]))
            .collect();
        if !kept.is_empty() {
            allowed = kept;
        }
    }
    let cooling = |w: &usize| {
        recency
            .work_ago
            .get(w)
            .is_some_and(|&ago| ago < config.work_cooldown_plays)
            || matches!(
                (ctx.now, recency.work_at.get(w)),
                (Some(now), Some(&at)) if now.saturating_sub(at) < config.work_cooldown_secs
            )
    };
    let fresh: Vec<usize> = allowed.iter().copied().filter(|w| !cooling(w)).collect();
    if fresh.len() >= (allowed.len() / 10).max(1) {
        return Eligible {
            works: fresh,
            rotation: false,
        };
    }
    let quarter = (allowed.len() / 4).max(1);
    allowed.sort_by_key(|w| Reverse(recency.work_ago.get(w).copied().unwrap_or(usize::MAX)));
    allowed.truncate(quarter);
    Eligible {
        works: allowed,
        rotation: true,
    }
}

const SCALE: u64 = 1_000_000_000;

/// A work's weight; with `factors`, also record every non-neutral factor.
fn weigh(
    pool: &WorkPool,
    w: usize,
    recency: &Recency<'_>,
    target: Option<u8>,
    config: &DjConfig,
    feedback: Option<&FeedbackModel>,
    mut factors: Option<&mut Vec<(Factor, i32)>>,
) -> u64 {
    let work = &pool.works[w];
    let mut weight = SCALE;
    let mut apply = |factor: Factor, per_mille: u64| {
        if per_mille != 1000 {
            weight = weight.saturating_mul(per_mille) / 1000;
            if let Some(factors) = factors.as_mut() {
                factors.push((factor, i32::try_from(per_mille).unwrap_or(i32::MAX)));
            }
        }
    };
    let composer = work.composer_key();
    // 1/√(composer's works): a composer's share grows with √works.
    let works = as_u64(
        pool.composer_works
            .get(composer)
            .copied()
            .unwrap_or(1)
            .max(1),
    );
    apply(
        Factor::ComposerBalance,
        1_000_000 / (works * 1_000_000).isqrt(),
    );
    apply(
        Factor::ComposerSpacing,
        spacing_pm(
            recency.composer_ago.get(composer).copied(),
            config.composer_spacing,
        ),
    );
    apply(
        Factor::AlbumSpacing,
        spacing_pm(
            recency.album_ago.get(work.album_key.as_str()).copied(),
            config.album_spacing,
        ),
    );
    if let Some(&ago) = recency.work_ago.get(&w) {
        apply(
            Factor::Recency,
            staleness_pm(ago, config.work_cooldown_plays),
        );
    }
    let (period_factor, period) = period_pm(work.period, recency);
    apply(period_factor, period);
    if let Some(target) = target {
        apply(Factor::EnergyFit, energy_fit_pm(work.energy(), target));
    }
    if let Some(last) = recency.last_energy {
        apply(Factor::EnergyJump, energy_jump_pm(work.energy(), last));
    }
    if work.is_liked() {
        apply(Factor::Liked, config.liked_boost_pm);
    }
    if work.completeness != Completeness::Complete {
        apply(Factor::Incomplete, config.incomplete_pm);
    }
    // Preferences outrank feedback: a favored or pinned work's feedback can
    // lift it further but never below neutral.
    let verdict = pool.verdicts[w];
    apply(Factor::Preference, verdict.weight_pm());
    if let Some(model) = feedback {
        let pm = model.multiplier_pm(work);
        let pm = if verdict.shields_feedback() {
            pm.max(1000)
        } else {
            pm
        };
        apply(Factor::Feedback, pm);
    }
    weight
}

/// A key heard `ago` work plays back keeps ((ago+1)/(spacing+1))² of its
/// weight until it is `spacing` plays old: at the default composer spacing of
/// 4 the previous work's composer keeps 4%, one heard three works back 64%.
fn spacing_pm(ago: Option<usize>, spacing: usize) -> u64 {
    match ago {
        Some(ago) if ago < spacing => {
            let (num, den) = (as_u64(ago + 1), as_u64(spacing + 1));
            (num * num * 1000 / (den * den)).max(1)
        }
        _ => 1000,
    }
}

/// Damp a period already on a run; favor one absent from the last few works.
/// Unknown periods are neutral.
fn period_pm(period: Period, recency: &Recency<'_>) -> (Factor, u64) {
    if period == Period::Unknown {
        return (Factor::PeriodRun, 1000);
    }
    match recency.period_run {
        Some((p, run)) if p == period => (
            Factor::PeriodRun,
            match run {
                1 => 550,
                2 => 250,
                _ => 100,
            },
        ),
        _ if !recency.recent_periods.is_empty() && !recency.recent_periods.contains(&period) => {
            (Factor::PeriodAbsent, 1400)
        }
        _ => (Factor::PeriodRun, 1000),
    }
}

/// Closeness to the energy target: 1000 on target, 810 at ±10, 360 at ±20,
/// and the floor of 30 from ±30 out, so nothing is ever impossible. Steep on
/// purpose: most works sit mid-range, and a gentler curve lets the spacing
/// factors wash out the time of day (a late-night set drifting to 35 against
/// a target of 25).
fn energy_fit_pm(energy: u8, target: u8) -> u64 {
    let d = u64::from(energy.abs_diff(target));
    let near = 1000u64.saturating_sub(d * d);
    (near * near / 1000).max(30)
}

/// Penalize jarring jumps between consecutive works (a fiery Allegro
/// opening straight after a Nocturne); steps up to 35 are free.
fn energy_jump_pm(energy: u8, last: u8) -> u64 {
    let jump = u64::from(energy.abs_diff(last));
    if jump <= 35 {
        1000
    } else {
        1000u64.saturating_sub((jump - 35) * 30).max(150)
    }
}

/// Works heard before stay de-weighted after their cooldown: 50% at zero
/// plays ago, recovering linearly to full weight at three cooldowns.
fn staleness_pm(ago: usize, cooldown: usize) -> u64 {
    if cooldown == 0 {
        return 1000;
    }
    (500 + 500 * as_u64(ago) / (3 * as_u64(cooldown))).min(1000)
}

fn draw(weights: &[u64], rng: &mut Rng) -> usize {
    let total = weights.iter().fold(0u64, |acc, &w| acc.saturating_add(w));
    if total == 0 {
        return rng.below(weights.len());
    }
    let mut r = rng.below_u64(total);
    for (i, &w) in weights.iter().enumerate() {
        if r < w {
            return i;
        }
        r -= w;
    }
    weights.len() - 1
}

/// The energy a pick aimed for, and whether it is the owner's preferred
/// energy (rather than the time of day's).
#[derive(Clone, Copy)]
struct Target {
    energy: Option<u8>,
    preferred: bool,
}

/// The readable line of a [`PickReason`]: who and when, then whatever else
/// shaped the pick, in a fixed order.
fn summarize(
    work: &Work,
    recency: &Recency<'_>,
    ctx: &PickContext<'_>,
    target: Target,
    steer: Option<&Steer>,
    factors: &[(Factor, i32)],
    relaxed: &[Relaxation],
) -> String {
    let has = |factor: Factor| factors.iter().any(|&(f, _)| f == factor);
    // "Brahms", but "Miles Davis" and "The Beatles" in full.
    let who = if work.movements.first().is_some_and(|m| m.known_composer) {
        surname(&work.composer)
    } else {
        work.composer.as_str()
    };
    let key = work.composer_key();
    let mut parts = vec![match (
        recency.composer_ago.get(key),
        recency.composer_at.get(key),
        ctx.now,
    ) {
        (None, _, _) => format!("{who} not heard recently"),
        (Some(_), Some(&at), Some(now)) => heard(who, now.saturating_sub(at)),
        (Some(0), _, _) => format!("more {who}"),
        (Some(&ago), _, _) => format!("{who} last heard {ago} works ago"),
    }];
    if has(Factor::PeriodAbsent) {
        parts.push(format!("balancing toward {}", work.period.label()));
    }
    if let Some(energy) = target.energy {
        let mood = match energy {
            0..=30 => "calm",
            31..=45 => "gentle",
            46..=55 => "steady",
            _ => "lively",
        };
        parts.push(match (ctx.energy_target, ctx.local_hour) {
            _ if target.preferred => format!("{mood} energy, as you prefer"),
            (None, Some(hour)) => format!("{mood} {} target", part_of_day(hour)),
            _ => format!("{mood} energy requested"),
        });
    }
    match steer {
        Some(Steer {
            mood: Some(mood), ..
        }) => parts.push(format!("{mood} mood")),
        Some(s) if s.constraints.energy_bias < 0 => parts.push("steered calmer".to_owned()),
        Some(s) if s.constraints.energy_bias > 0 => parts.push("steered brighter".to_owned()),
        _ => {}
    }
    if let Some(&(_, pm)) = factors.iter().find(|(f, _)| *f == Factor::Preference) {
        parts.push(
            if pm >= 3000 {
                "pinned in your preferences"
            } else {
                "favored in your preferences"
            }
            .to_owned(),
        );
    }
    if let Some(&(_, pm)) = factors.iter().find(|(f, _)| *f == Factor::Feedback) {
        let note = if pm > 1000 {
            "favored by your feedback"
        } else {
            "played less after your feedback"
        };
        parts.push(note.to_owned());
    }
    for (factor, note) in [
        (Factor::Liked, "from your liked tracks"),
        (
            Factor::Incomplete,
            "only some movements are in your library",
        ),
        (
            Factor::Rotation,
            "rotating through the least recently played",
        ),
        (Factor::LongWork, "a long work, allowed this time"),
        (
            Factor::Avoided,
            "everything else left is avoided in your preferences",
        ),
    ] {
        if has(factor) {
            parts.push(note.to_owned());
        }
    }
    if let Some((last, rest)) = relaxed.split_last() {
        let names = if rest.is_empty() {
            last.label().to_owned()
        } else {
            let rest: Vec<&str> = rest.iter().map(|r| r.label()).collect();
            format!("{} and {}", rest.join(", "), last.label())
        };
        let noun = if rest.is_empty() { "filter" } else { "filters" };
        parts.push(format!(
            "relaxed the {names} {noun} (too few matching works)"
        ));
    }
    parts.join("; ")
}

fn heard(who: &str, secs: i64) -> String {
    let hours = secs / 3600;
    if hours >= 48 {
        format!("{who} not heard in {} days", hours / 24)
    } else if hours >= 2 {
        format!("{who} not heard in {hours} hours")
    } else {
        format!("{who} heard {} minutes ago", secs / 60)
    }
}

fn part_of_day(hour: u8) -> &'static str {
    match hour % 24 {
        0..=4 => "late-night",
        5..=11 => "morning",
        12..=16 => "afternoon",
        17..=20 => "evening",
        _ => "late-evening",
    }
}

/// "Johannes Brahms" → "Brahms"; "Johann Strauss II" → "Strauss".
fn surname(name: &str) -> &str {
    let mut words = name.split_whitespace().rev();
    let last = words.next().unwrap_or(name);
    if matches!(last, "II" | "III" | "Jr" | "Jr." | "Sr" | "Sr.") {
        words.next().unwrap_or(last)
    } else {
        last
    }
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss)]
mod tests {
    use std::collections::{BTreeMap, HashSet};
    use std::time::Instant;

    use super::*;
    use crate::library::{LibraryItem, Origin};
    use crate::test_shelf::*;
    use crate::works::movement_number;

    #[test]
    fn rng_is_reproducible_bounded_and_seed_sensitive() {
        let draws = |seed| {
            let mut rng = Rng::new(seed);
            (0..64).map(|_| rng.below(10)).collect::<Vec<_>>()
        };
        assert_eq!(draws(7), draws(7));
        assert_ne!(draws(7), draws(8));
        assert!(draws(0).iter().all(|&d| d < 10), "seed 0 is valid too");
        let mut rng = Rng::new(3);
        let mut hist = [0usize; 4];
        for _ in 0..40_000 {
            hist[rng.below(4)] += 1;
        }
        assert!(
            hist.iter().all(|&h| (9_000..11_000).contains(&h)),
            "{hist:?}"
        );
    }

    #[test]
    fn a_library_without_classical_music_plays_spread_across_artists() {
        let pool = works_of(&song_items());
        assert_eq!(pool.len(), 25, "every song but the explicit one");
        let artists: HashSet<&str> = pool.works().iter().map(Work::composer_key).collect();
        assert_eq!(artists.len(), 6);
        let config = DjConfig::default();
        for seed in 1..=6 {
            let picks = simulate(&pool, &config, seed, 60, Some(20));
            let log = || transcript(seed, &pool, &picks);
            assert_eq!(picks.len(), 60, "{}", log());
            let heard: HashSet<&str> = picks.iter().map(|p| p.work.composer_key()).collect();
            assert_eq!(heard, artists, "{}", log());
            let repeats = picks
                .windows(2)
                .filter(|pair| pair[0].work.composer_key() == pair[1].work.composer_key())
                .count();
            assert!(repeats <= 4, "{repeats} artists back to back\n{}", log());
            for p in &picks {
                assert_eq!(p.movements.len(), 1, "{}", log());
                // Named in full: "Nina Marsh Quartet", not "Quartet".
                assert!(
                    p.reason.summary.starts_with(&p.work.composer),
                    "{}",
                    p.reason.summary
                );
            }
        }
    }

    #[test]
    fn a_mixed_library_plays_songs_and_whole_classical_works() {
        let pool = works_of(&mixed_items());
        let picks = simulate(&pool, &DjConfig::default(), 3, 40, Some(10));
        let log = || transcript(3, &pool, &picks);
        let (classical, songs): (Vec<_>, Vec<_>) =
            picks.iter().partition(|p| p.movements[0].classical);
        assert!(!classical.is_empty() && !songs.is_empty(), "{}", log());
        for p in classical {
            assert_eq!(p.movements, p.work.movements.as_slice());
            let surname = p.work.composer.rsplit(' ').next().unwrap();
            assert!(
                p.reason.summary.starts_with(surname),
                "{}",
                p.reason.summary
            );
        }
        assert!(
            picks
                .iter()
                .any(|p| p.work.composer == "Ludwig van Beethoven" && p.movements.len() == 4),
            "{}",
            log()
        );
    }

    #[test]
    fn empty_pool_yields_nothing() {
        let pool = WorkPool::default();
        let (ctx, config) = (PickContext::default(), DjConfig::default());
        let mut rng = Rng::new(1);
        assert!(pick_next(&pool, &ctx, &config, &mut rng).is_none());
        assert_eq!(plan(&pool, &ctx, &config, 5, &mut rng).len(), 0);
    }

    #[test]
    fn shelf_groups_into_works() {
        let pool = works_of(&shelf_items(1));
        assert_eq!(pool.len(), 111);
        let tracks: usize = pool.works().iter().map(|w| w.movements.len()).sum();
        assert_eq!(tracks, 291);
        let cantata = pool.work_of("spotify:track:0-1-2").unwrap();
        assert_eq!(cantata.movements.len(), 4);
        assert_eq!(cantata.composer, "Johann Sebastian Bach");
    }

    #[test]
    fn long_run_plays_whole_works_with_spread() {
        // Three times the shelf: with 111 works, round-the-clock listening and
        // a 24 h cooldown force a near-cycle whose mix is the shelf's own, so
        // composer balance shows on a library the cooldown doesn't exhaust.
        let pool = works_of(&shelf_items(3));
        assert_eq!(pool.len(), 333);
        let config = DjConfig::default();
        let composer = |w: &Work| w.composer_key().to_owned();
        let bach = |p: &[&Work]| {
            p.iter()
                .filter(|w| w.composer == "Johann Sebastian Bach")
                .count() as f64
                / p.len() as f64
        };
        for seed in [42, 7, 2026] {
            let picks = simulate(&pool, &config, seed, 500, None);
            // Never split, always in order: the plan is the work's movements.
            for planned in &picks {
                assert!(std::ptr::eq(
                    planned.movements,
                    planned.work.movements.as_slice()
                ));
                let numerals: Vec<u32> = planned
                    .movements
                    .iter()
                    .filter_map(|m| m.movement.as_deref().and_then(movement_number))
                    .collect();
                assert!(numerals.windows(2).all(|p| p[0] < p[1]), "{numerals:?}");
            }
            let dj = works(&picks);
            let base = uniform(&pool, seed, 500, 5);
            let (dj_rep, base_rep) = (repeat_rate(&dj, composer), repeat_rate(&base, composer));
            let dj_comp = distinct_per_window(&dj, 8, composer);
            let base_comp = distinct_per_window(&base, 8, composer);
            let metrics = format!(
                "seed {seed}: composer repeat {dj_rep:.3} (uniform {base_rep:.3}), \
                 distinct composers/8 {dj_comp:.2} (uniform {base_comp:.2}), \
                 Bach share {:.3} (uniform {:.3}), longest period run {}, min work gap {}",
                bach(&dj),
                bach(&base),
                longest_run(&dj, |w| w.period),
                min_work_gap(&dj),
            );
            eprintln!("{metrics}");
            // On failure: the metrics and the whole planned sequence.
            let log = || format!("{metrics}\n{}", transcript(seed, &pool, &picks));
            assert!(min_work_gap(&dj) >= config.work_cooldown_plays, "{}", log());
            assert!(dj_rep <= 0.03, "{}", log());
            assert!(dj_rep * 3.0 < base_rep, "{}", log());
            // Uniform over works is already spread (single pieces are works).
            assert!(dj_comp >= 6.8 && dj_comp > base_comp + 0.5, "{}", log());
            assert!(bach(&dj) < 0.75 * 20.0 / 111.0, "{}", log());
            assert!(longest_run(&dj, |w| w.period) <= 3, "{}", log());
            let heard: HashSet<&str> = dj.iter().map(|w| w.composer.as_str()).collect();
            assert_eq!(heard.len(), SHELF.len(), "{}", log());
        }
    }

    #[test]
    fn long_works_stay_out_unless_allowed() {
        let mut items = shelf_items(1);
        items.extend(opera());
        let pool = works_of(&items);
        let traviata = pool.work_of("spotify:track:traviata-1").unwrap();
        assert_eq!((traviata.movements.len(), traviata.total_secs), (24, 7200));

        let picks = simulate(&pool, &DjConfig::default(), 5, 500, None);
        assert!(picks.iter().all(|p| !std::ptr::eq(p.work, traviata)));

        let config = DjConfig {
            allow_long_works: true,
            ..DjConfig::default()
        };
        let picks = simulate(&pool, &config, 5, 500, None);
        let operas: Vec<&PlannedWork<'_>> = picks
            .iter()
            .filter(|p| std::ptr::eq(p.work, traviata))
            .collect();
        assert!(!operas.is_empty(), "allowed long works get played");
        assert!(
            operas
                .iter()
                .all(|p| p.reason.has(Factor::LongWork) && p.movements.len() == 24)
        );

        // A library of nothing but long works still plays.
        let only = works_of(&opera());
        let ctx = PickContext::default();
        let planned = pick_next(&only, &ctx, &DjConfig::default(), &mut Rng::new(1)).unwrap();
        assert!(planned.reason.has(Factor::LongWork));
        assert!(
            planned
                .reason
                .summary
                .contains("a long work, allowed this time")
        );
    }

    #[test]
    fn incomplete_works_still_make_a_full_set() {
        // Liked Songs only, every four-movement work missing its finale.
        let items: Vec<LibraryItem> = shelf_items(1)
            .into_iter()
            .filter(|i| !i.title.ends_with("IV. Presto"))
            .map(|mut i| {
                i.origin = Origin::LikedTrack;
                i
            })
            .collect();
        let pool = works_of(&items);
        let ctx = PickContext::default();
        let picks = plan(&pool, &ctx, &DjConfig::default(), 40, &mut Rng::new(11));
        assert_eq!(picks.len(), 40);
        let distinct: HashSet<*const Work> =
            picks.iter().map(|p| std::ptr::from_ref(p.work)).collect();
        assert_eq!(distinct.len(), 40, "no repeats within the cooldown");
        let partial = picks
            .iter()
            .find(|p| p.reason.has(Factor::Incomplete))
            .expect("partial works play");
        assert!(
            partial
                .reason
                .summary
                .contains("only some movements are in your library")
        );
        assert!(partial.reason.summary.contains("from your liked tracks"));
    }

    #[test]
    fn energy_follows_the_time_of_day() {
        let pool = works_of(&shelf_items(1));
        let config = DjConfig::default();
        // A night's (or a morning's) listening, ~24 works, over three seeds.
        // Much longer and the 24 h cooldown exhausts the shelf's ~40 calm
        // works, forcing livelier ones whatever the target.
        let session = |hour: Option<u8>| -> Vec<&Work> {
            [9, 10, 11]
                .into_iter()
                .flat_map(|seed| works(&simulate(&pool, &config, seed, 24, hour)))
                .collect()
        };
        let (night, morning) = (session(Some(23)), session(Some(10)));
        let base: Vec<&Work> = [9, 10, 11]
            .into_iter()
            .flat_map(|seed| uniform(&pool, seed, 24, 5))
            .collect();
        let miss = |picks: &[&Work], hour| {
            let target = energy_target_for_hour(hour);
            picks
                .iter()
                .map(|w| f64::from(w.energy().abs_diff(target)))
                .sum::<f64>()
                / picks.len() as f64
        };
        eprintln!(
            "work energy: night {:.1}, morning {:.1}; target miss night {:.1} (uniform {:.1}), \
             morning {:.1} (uniform {:.1})",
            mean_energy(&night),
            mean_energy(&morning),
            miss(&night, 23),
            miss(&base, 23),
            miss(&morning, 10),
            miss(&base, 10)
        );
        assert!(mean_energy(&night) + 10.0 < mean_energy(&morning));
        assert!(miss(&night, 23) < 0.7 * miss(&base, 23));
        assert!(miss(&morning, 10) < miss(&base, 10));
    }

    #[test]
    fn same_seed_same_set() {
        let pool = works_of(&shelf_items(1));
        let config = DjConfig::default();
        let keys = |seed| {
            simulate(&pool, &config, seed, 100, None)
                .iter()
                .map(|p| p.work.work_key.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(keys(1234), keys(1234));
        assert_ne!(keys(1234), keys(1235));
    }

    #[test]
    fn plan_continues_history_without_repeats() {
        let pool = works_of(&shelf_items(1));
        let config = DjConfig::default();
        let mut rng = Rng::new(77);
        let first = plan(&pool, &PickContext::default(), &config, 20, &mut rng);
        let history: Vec<PlayRecord> = first
            .iter()
            .flat_map(|p| p.movements.iter())
            .map(|m| PlayRecord::new(m.track.source_uri.clone()))
            .collect();
        let ctx = PickContext {
            history: &history,
            ..PickContext::default()
        };
        let next = plan(&pool, &ctx, &config, 20, &mut rng);
        let all: HashSet<*const Work> = first
            .iter()
            .chain(&next)
            .map(|p| std::ptr::from_ref(p.work))
            .collect();
        assert_eq!(all.len(), 40);
    }

    fn tiny(composers: &[&str]) -> WorkPool {
        let items: Vec<LibraryItem> = composers
            .iter()
            .enumerate()
            .map(|(i, composer)| {
                let album = (format!("Album {i}"), format!("spotify:album:tiny-{i}"));
                let uri = format!("spotify:track:tiny-{i}");
                item(
                    uri,
                    format!("Sonata No. {i}"),
                    composer,
                    album,
                    300,
                    Origin::SavedAlbum,
                )
            })
            .collect();
        works_of(&items)
    }

    #[test]
    fn small_pool_rotates_instead_of_repeating() {
        let pool = tiny(&["Joseph Haydn", "Franz Schubert", "Claude Debussy"]);
        let picks = simulate(&pool, &DjConfig::default(), 3, 30, Some(12));
        let dj = works(&picks);
        assert_eq!(repeat_rate(&dj, |w| w.work_key.clone()), 0.0);
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for w in &dj {
            *counts.entry(w.work_key.as_str()).or_default() += 1;
        }
        assert!(counts.values().all(|&n| n == 10), "{counts:?}");
        let rotated = picks
            .iter()
            .filter(|p| p.reason.has(Factor::Rotation))
            .count();
        assert_eq!(rotated, 27, "every pick after the first three rotates");
        assert!(
            picks[5]
                .reason
                .summary
                .contains("rotating through the least recently played")
        );
    }

    #[test]
    fn a_work_counts_as_played_when_any_movement_plays() {
        let pool = works_of(&shelf_items(1));
        let symphony = pool.work_of("spotify:track:5-1-0").unwrap(); // Beethoven's first
        // Only its third movement was heard, an hour ago, then 40 other works.
        let mut history = vec![PlayRecord::at("spotify:track:5-1-2", MIDNIGHT)];
        history.extend((0..40).map(|i| PlayRecord::new(format!("spotify:track:manual-{i}"))));
        let ctx = PickContext {
            history: &history,
            now: Some(MIDNIGHT + 3600),
            ..PickContext::default()
        };
        let config = DjConfig {
            work_cooldown_plays: 0,
            ..DjConfig::default()
        };
        let ever = (0..300).any(|seed| {
            let planned = pick_next(&pool, &ctx, &config, &mut Rng::new(seed)).unwrap();
            std::ptr::eq(planned.work, symphony)
        });
        assert!(!ever, "the whole symphony is on its 24 h cooldown");
    }

    #[test]
    fn liked_works_are_favored() {
        let items: Vec<LibraryItem> = [Origin::LikedTrack, Origin::SavedAlbum]
            .into_iter()
            .enumerate()
            .map(|(i, origin)| {
                let album = (format!("Album {i}"), format!("spotify:album:twin-{i}"));
                let uri = format!("spotify:track:twin-{i}");
                item(
                    uri,
                    format!("Nocturne No. {i}"),
                    "Frédéric Chopin",
                    album,
                    300,
                    origin,
                )
            })
            .collect();
        let pool = works_of(&items);
        let (ctx, config) = (PickContext::default(), DjConfig::default());
        let mut rng = Rng::new(8);
        let liked = (0..4000)
            .filter(|_| {
                pick_next(&pool, &ctx, &config, &mut rng)
                    .unwrap()
                    .work
                    .is_liked()
            })
            .count();
        // Expected 1.3 : 1 → ~56.5% liked.
        assert!((2140..2380).contains(&liked), "liked picked {liked}/4000");
    }

    /// Six works: two Brahms symphonies and four from other eras.
    fn snapshot_library() -> Vec<LibraryItem> {
        let work = |id: &str, composer: &str, title: &str, movements: &[&str]| {
            movements
                .iter()
                .enumerate()
                .map(|(m, movement)| {
                    let album = (
                        format!("{composer}: {title}"),
                        format!("spotify:album:{id}"),
                    );
                    let uri = format!("spotify:track:{id}-{m}");
                    let title = format!("{title}: {movement}");
                    item(uri, title, composer, album, 600, Origin::SavedAlbum)
                })
                .collect::<Vec<_>>()
        };
        let four = [
            "I. Allegro con brio",
            "II. Andante",
            "III. Poco allegretto",
            "IV. Allegro",
        ];
        let brahms = "Johannes Brahms";
        let mut items = work(
            "brahms3",
            brahms,
            "Symphony No. 3 in F Major, Op. 90",
            &four,
        );
        items.extend(work(
            "brahms4",
            brahms,
            "Symphony No. 4 in E Minor, Op. 98",
            &four,
        ));
        items.extend(work(
            "bach",
            "Johann Sebastian Bach",
            "Cello Suite No. 1 in G Major, BWV 1007",
            &["I. Prélude", "II. Allemande"],
        ));
        items.extend(work(
            "mozart",
            "Wolfgang Amadeus Mozart",
            "Piano Sonata No. 11 in A Major, K. 331",
            &[
                "I. Andante grazioso",
                "II. Menuetto",
                "III. Alla Turca: Allegretto",
            ],
        ));
        items.extend(work(
            "haydn",
            "Joseph Haydn",
            "String Quartet in C Major, Op. 76 No. 3",
            &["I. Allegro", "II. Poco adagio"],
        ));
        items.extend(work(
            "vivaldi",
            "Antonio Vivaldi",
            "Violin Concerto in E Major, RV 269",
            &["I. Allegro", "II. Largo", "III. Allegro"],
        ));
        items
    }

    /// A pick whose reason is fully determined: only one work is out of
    /// cooldown.
    #[test]
    fn pick_reason_snapshot() {
        let pool = works_of(&snapshot_library());

        let now = MIDNIGHT + 19 * 3600;
        let mut history = Vec::new();
        for (id, movements, at) in [
            ("brahms4", 4, now - 4 * 86_400 - 3600),
            ("vivaldi", 3, now - 3000),
            ("haydn", 2, now - 2400),
            ("bach", 2, now - 1800),
            ("mozart", 3, now - 1200),
        ] {
            history.extend(
                (0..movements).map(|m| PlayRecord::at(format!("spotify:track:{id}-{m}"), at)),
            );
        }
        let ctx = PickContext {
            history: &history,
            now: Some(now),
            local_hour: Some(19),
            energy_target: None,
            steer: None,
            feedback: None,
        };
        let planned = pick_next(&pool, &ctx, &DjConfig::default(), &mut Rng::new(1)).unwrap();
        assert_eq!(planned.work.title, "Symphony No. 3 in F Major, Op. 90");
        assert_eq!(planned.movements.len(), 4);
        assert_eq!(
            planned.reason.factors,
            [
                (Factor::ComposerBalance, 707),
                (Factor::PeriodAbsent, 1400),
                (Factor::EnergyFit, 30),
                // Mozart's Andante (40) to Brahms's Allegro con brio (82).
                (Factor::EnergyJump, 790),
            ]
        );
        assert_eq!(
            planned.reason.summary,
            "Brahms not heard in 4 days; balancing toward late-Romantic; gentle evening target"
        );

        // Untimed history reads in works; an explicit target reads as requested.
        let untimed: Vec<PlayRecord> = history
            .iter()
            .map(|p| PlayRecord::new(p.source_uri.clone()))
            .collect();
        let ctx = PickContext {
            history: &untimed,
            now: None,
            local_hour: None,
            energy_target: Some(80),
            steer: None,
            feedback: None,
        };
        let planned = pick_next(&pool, &ctx, &DjConfig::default(), &mut Rng::new(1)).unwrap();
        assert_eq!(
            planned.reason.summary,
            "Brahms last heard 4 works ago; balancing toward late-Romantic; lively energy requested"
        );
    }

    #[test]
    fn planning_over_a_10k_track_library_is_fast() {
        let items = shelf_items(35);
        assert!(items.len() >= 10_000, "{} tracks", items.len());
        let pool = works_of(&items);
        let config = DjConfig::default();
        let mut history = Vec::new();
        for planned in plan(
            &pool,
            &PickContext::default(),
            &config,
            100,
            &mut Rng::new(1),
        ) {
            history.extend(
                planned
                    .movements
                    .iter()
                    .map(|m| PlayRecord::new(m.track.source_uri.clone())),
            );
        }
        let ctx = PickContext {
            history: &history,
            now: Some(MIDNIGHT),
            local_hour: Some(20),
            energy_target: None,
            steer: None,
            feedback: None,
        };
        let started = Instant::now();
        let planned = plan(&pool, &ctx, &config, 5, &mut Rng::new(2));
        let elapsed = started.elapsed();
        eprintln!(
            "plan(5) over {} works / {} tracks with {} history rows: {elapsed:?}",
            pool.len(),
            items.len(),
            history.len()
        );
        assert_eq!(planned.len(), 5);
        // The budget is 50 ms in an optimized build; unoptimized test builds
        // run several times slower, so they get generous headroom.
        let budget_ms = if cfg!(debug_assertions) { 1000 } else { 50 };
        assert!(elapsed.as_millis() < budget_ms, "{elapsed:?}");
    }

    #[test]
    fn weight_curves() {
        assert_eq!(spacing_pm(None, 4), 1000);
        assert_eq!(spacing_pm(Some(4), 4), 1000);
        assert_eq!(spacing_pm(Some(0), 4), 40);
        assert_eq!(spacing_pm(Some(3), 4), 640);
        assert_eq!(spacing_pm(Some(3), 0), 1000);
        assert_eq!(energy_fit_pm(50, 50), 1000);
        assert_eq!(energy_fit_pm(60, 50), 810);
        assert_eq!(energy_fit_pm(70, 50), 360);
        assert_eq!(energy_fit_pm(90, 10), 30);
        assert_eq!(energy_jump_pm(20, 55), 1000);
        assert_eq!(energy_jump_pm(10, 60), 550);
        assert_eq!(energy_jump_pm(5, 95), 150);
        assert_eq!(staleness_pm(0, 40), 500);
        assert_eq!(staleness_pm(120, 40), 1000);
        assert_eq!(staleness_pm(9, 0), 1000);
        assert!((0..24).all(|h| energy_target_for_hour(h) <= 60));
        assert!(energy_target_for_hour(2) < energy_target_for_hour(10));
        assert_eq!(surname("Johannes Brahms"), "Brahms");
        assert_eq!(surname("Johann Strauss II"), "Strauss");
        assert_eq!(heard("Bach", 90 * 60), "Bach heard 90 minutes ago");
        assert_eq!(heard("Bach", 5 * 3600), "Bach not heard in 5 hours");
    }
}
