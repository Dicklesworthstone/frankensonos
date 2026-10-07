//! The classical-music DJ: pure selection logic.
//!
//! Given the [`CandidatePool`] (the owner's classical tracks, analysed by
//! [`crate::classical`]) and the play history, pick the next track for
//! pleasant variety:
//!
//! * **Anti-repeat.** A track never recurs within
//!   [`DjConfig::track_cooldown_plays`] plays or
//!   [`DjConfig::track_cooldown_secs`] seconds, and tracks heard a while ago
//!   stay de-weighted until they are well clear of the cooldown. If the
//!   cooldown leaves too few tracks (a small pool, a long session), the DJ
//!   rotates through the least-recently-played slice instead of repeating.
//! * **Spread.** The same composer, work, or album is strongly de-weighted
//!   for a few plays and recovers quadratically; same-period runs are damped
//!   and periods missing from the last few plays are favored; prolific
//!   composers are damped (weight ∝ 1/√tracks) so a 300-track Bach shelf
//!   doesn't drown out five Fauré pieces.
//! * **Energy.** Prefer tracks near the time-of-day target (calm late at
//!   night, livelier mid-morning) and avoid jarring jumps from the previous
//!   track.
//!
//! Weights are integer per-mille factors and every iteration runs in pool or
//! history order, so a seeded [`Rng`] reproduces a set exactly anywhere.

use std::cmp::Reverse;
use std::collections::HashMap;

use crate::classical::{CandidatePool, ClassicalTrack, Period};

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

/// One entry of play history (the store's `play_history` row).
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

/// Tuning for [`pick_next`]. Spacings count plays back from the most recent
/// (0 = the previous track); weights are per-mille (1000 = neutral).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DjConfig {
    /// Hard: a track can't recur within this many plays…
    pub track_cooldown_plays: usize,
    /// …nor within this many seconds (when history carries timestamps).
    pub track_cooldown_secs: i64,
    /// Soft: plays over which a composer recovers from being heard.
    pub composer_spacing: usize,
    /// Soft: plays over which a work (all its movements) recovers.
    pub work_spacing: usize,
    /// Soft: plays over which an album (≈ the same performers) recovers.
    pub album_spacing: usize,
    /// How many recent pool plays count when favoring absent periods.
    pub period_memory: usize,
    /// How much history to scan; older plays are ignored.
    pub history_horizon: usize,
    /// Weight for tracks the owner individually liked.
    pub liked_boost_pm: u64,
}

impl Default for DjConfig {
    fn default() -> Self {
        Self {
            track_cooldown_plays: 150,
            track_cooldown_secs: 24 * 60 * 60,
            composer_spacing: 8,
            work_spacing: 30,
            album_spacing: 8,
            period_memory: 6,
            history_horizon: 2000,
            liked_boost_pm: 1300,
        }
    }
}

/// What the DJ knows about "now" when picking.
#[derive(Debug, Clone, Copy, Default)]
pub struct PickContext<'h> {
    /// Play history, most recent last.
    pub history: &'h [PlayRecord],
    /// Current Unix time (enables the time-based cooldown).
    pub now: Option<i64>,
    /// Local hour 0–23 (enables the time-of-day energy target).
    pub local_hour: Option<u8>,
    /// Explicit energy target 0–100; overrides the time of day.
    pub energy_target: Option<u8>,
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

/// Pick the next track, or `None` for an empty pool. Deterministic for a
/// given pool, context, config and RNG state.
#[must_use]
pub fn pick_next<'p>(
    pool: &'p CandidatePool,
    ctx: &PickContext<'_>,
    config: &DjConfig,
    rng: &mut Rng,
) -> Option<&'p ClassicalTrack> {
    if pool.is_empty() {
        return None;
    }
    let recency = Recency::scan(pool, ctx.history, config);
    let target = ctx
        .energy_target
        .or_else(|| ctx.local_hour.map(energy_target_for_hour));
    let candidates = candidates(pool, &recency, ctx, config);
    let weights: Vec<u64> = candidates
        .iter()
        .map(|&i| weight(pool, i, &recency, target, config))
        .collect();
    Some(&pool.tracks()[candidates[draw(&weights, rng)]])
}

/// Pick `count` tracks ahead (to keep a queue fed); each pick sees the
/// earlier ones as just played.
#[must_use]
pub fn plan<'p>(
    pool: &'p CandidatePool,
    ctx: &PickContext<'_>,
    config: &DjConfig,
    count: usize,
    rng: &mut Rng,
) -> Vec<&'p ClassicalTrack> {
    let mut history = ctx.history.to_vec();
    let mut picks = Vec::with_capacity(count);
    for _ in 0..count {
        let step = PickContext {
            history: &history,
            ..*ctx
        };
        let Some(track) = pick_next(pool, &step, config, rng) else {
            break;
        };
        history.push(PlayRecord {
            source_uri: track.track.source_uri.clone(),
            played_at: ctx.now,
        });
        picks.push(track);
    }
    picks
}

/// What the recent history says about each pool track and grouping key.
#[derive(Default)]
struct Recency<'p> {
    /// Pool index → plays since it last played.
    track_ago: HashMap<usize, usize>,
    /// Pool index → when it last played (if timestamped).
    track_at: HashMap<usize, i64>,
    composer_ago: HashMap<&'p str, usize>,
    work_ago: HashMap<&'p str, usize>,
    album_ago: HashMap<&'p str, usize>,
    /// Period of the latest pool plays and how many in a row share it.
    period_run: Option<(Period, usize)>,
    /// Periods of the last `period_memory` pool plays.
    recent_periods: Vec<Period>,
    /// Energy of the previous track, if it was a pool track.
    last_energy: Option<u8>,
}

impl<'p> Recency<'p> {
    fn scan(pool: &'p CandidatePool, history: &[PlayRecord], config: &DjConfig) -> Self {
        let mut seen = Self::default();
        let mut run_open = true;
        let latest_first = history.iter().rev().take(config.history_horizon);
        for (ago, play) in latest_first.enumerate() {
            // Plays outside the pool (manual picks) still count as distance.
            let Some(index) = pool.index_of(&play.source_uri) else {
                continue;
            };
            let track = &pool.tracks()[index];
            seen.track_ago.entry(index).or_insert(ago);
            if let Some(at) = play.played_at {
                seen.track_at.entry(index).or_insert(at);
            }
            for (map, key) in [
                (&mut seen.composer_ago, track.composer_key.as_str()),
                (&mut seen.work_ago, track.work_key.as_str()),
                (&mut seen.album_ago, track.album_key.as_str()),
            ] {
                if !key.is_empty() {
                    map.entry(key).or_insert(ago);
                }
            }
            if ago == 0 {
                seen.last_energy = Some(track.energy);
            }
            if seen.recent_periods.len() < config.period_memory {
                seen.recent_periods.push(track.period);
            }
            if run_open {
                seen.period_run = match seen.period_run {
                    None => Some((track.period, 1)),
                    Some((period, run)) if period == track.period => Some((period, run + 1)),
                    other => {
                        run_open = false;
                        other
                    }
                };
            }
        }
        seen
    }

    fn ago(map: &HashMap<&'p str, usize>, key: &str) -> Option<usize> {
        map.get(key).copied()
    }
}

/// Pool indices eligible for this pick: everything out of cooldown, or — when
/// that leaves fewer than a tenth of the pool — the least-recently-played
/// quarter, so a small pool rotates rather than repeats.
fn candidates(
    pool: &CandidatePool,
    recency: &Recency<'_>,
    ctx: &PickContext<'_>,
    config: &DjConfig,
) -> Vec<usize> {
    let cooling = |i: &usize| {
        recency
            .track_ago
            .get(i)
            .is_some_and(|&ago| ago < config.track_cooldown_plays)
            || matches!(
                (ctx.now, recency.track_at.get(i)),
                (Some(now), Some(&at)) if now.saturating_sub(at) < config.track_cooldown_secs
            )
    };
    let fresh: Vec<usize> = (0..pool.len()).filter(|i| !cooling(i)).collect();
    if fresh.len() >= (pool.len() / 10).max(1) {
        return fresh;
    }
    let mut stalest: Vec<usize> = (0..pool.len()).collect();
    stalest.sort_by_key(|i| Reverse(recency.track_ago.get(i).copied().unwrap_or(usize::MAX)));
    stalest.truncate((pool.len() / 4).max(1));
    stalest
}

const SCALE: u64 = 1_000_000_000;

fn weight(
    pool: &CandidatePool,
    i: usize,
    recency: &Recency<'_>,
    target: Option<u8>,
    config: &DjConfig,
) -> u64 {
    let t = &pool.tracks()[i];
    // 1/√(composer's track count): composer share grows with √tracks.
    let size = as_u64(pool.composer_size(&t.composer_key).max(1));
    let mut w = SCALE * 1000 / size.saturating_mul(1_000_000).isqrt();
    w = scale(
        w,
        spacing_pm(
            Recency::ago(&recency.composer_ago, &t.composer_key),
            config.composer_spacing,
        ),
    );
    w = scale(
        w,
        spacing_pm(
            Recency::ago(&recency.work_ago, &t.work_key),
            config.work_spacing,
        ),
    );
    w = scale(
        w,
        spacing_pm(
            Recency::ago(&recency.album_ago, &t.album_key),
            config.album_spacing,
        ),
    );
    w = scale(w, period_pm(t.period, recency));
    if let Some(target) = target {
        w = scale(w, energy_fit_pm(t.energy, target));
    }
    if let Some(last) = recency.last_energy {
        w = scale(w, energy_jump_pm(t.energy, last));
    }
    if t.origin.is_liked() {
        w = scale(w, config.liked_boost_pm);
    }
    if let Some(&ago) = recency.track_ago.get(&i) {
        w = scale(w, staleness_pm(ago, config.track_cooldown_plays));
    }
    w
}

fn scale(w: u64, per_mille: u64) -> u64 {
    w.saturating_mul(per_mille) / 1000
}

/// A key heard `ago` plays back keeps ((ago+1)/(spacing+1))² of its weight
/// until it is `spacing` plays old: at the default composer spacing of 8 the
/// previous track's composer keeps ~1%, one heard four plays back ~31%.
fn spacing_pm(ago: Option<usize>, spacing: usize) -> u64 {
    match ago {
        Some(ago) if ago < spacing => {
            let (num, den) = (as_u64(ago + 1), as_u64(spacing + 1));
            (num * num * 1000 / (den * den)).max(1)
        }
        _ => 1000,
    }
}

/// Damp a period that is already on a run; favor one absent from the last
/// few plays. Unknown periods are neutral.
fn period_pm(period: Period, recency: &Recency<'_>) -> u64 {
    if period == Period::Unknown {
        return 1000;
    }
    match recency.period_run {
        Some((p, run)) if p == period => match run {
            1 => 550,
            2 => 250,
            _ => 100,
        },
        _ if !recency.recent_periods.is_empty() && !recency.recent_periods.contains(&period) => {
            1400
        }
        _ => 1000,
    }
}

/// Closeness to the energy target: 1000 on target, 800 at ±20, 200 at ±40,
/// floored at 80 so nothing is ever impossible.
fn energy_fit_pm(energy: u8, target: u8) -> u64 {
    let d = u64::from(energy.abs_diff(target));
    1000u64.saturating_sub(d * d / 2).max(80)
}

/// Penalize jarring jumps from the previous track (a Presto straight after
/// a Nocturne); steps up to 35 are free.
fn energy_jump_pm(energy: u8, last: u8) -> u64 {
    let jump = u64::from(energy.abs_diff(last));
    if jump <= 35 {
        1000
    } else {
        1000u64.saturating_sub((jump - 35) * 30).max(150)
    }
}

/// Tracks heard before stay de-weighted after their cooldown: 50% at zero
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

#[cfg(test)]
#[allow(clippy::cast_precision_loss)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet, HashSet};

    use super::*;
    use crate::library::{LibraryItem, Origin};

    /// A realistic, lopsided shelf: (composer, form, works, movements/work).
    const SHELF: &[(&str, &str, usize, usize)] = &[
        ("Johann Sebastian Bach", "Cantata", 20, 4),
        ("Antonio Vivaldi", "Concerto", 8, 3),
        ("George Frideric Handel", "Concerto Grosso", 5, 3),
        ("Wolfgang Amadeus Mozart", "Piano Sonata", 12, 3),
        ("Joseph Haydn", "String Quartet", 4, 4),
        ("Ludwig van Beethoven", "Symphony", 9, 4),
        ("Franz Schubert", "Impromptu", 8, 1),
        ("Frédéric Chopin", "Nocturne", 15, 1),
        ("Johannes Brahms", "Symphony", 4, 4),
        ("Pyotr Ilyich Tchaikovsky", "Symphony", 3, 4),
        ("Gabriel Fauré", "Barcarolle", 3, 1),
        ("Claude Debussy", "Prélude", 6, 1),
        ("Maurice Ravel", "Piano Concerto", 2, 3),
        ("Erik Satie", "Gymnopédie", 3, 1),
        ("Dmitri Shostakovich", "String Quartet", 2, 4),
        ("Arvo Pärt", "Fratres", 3, 1),
        ("Hildegard von Bingen", "Antiphon", 2, 1),
        ("Thomas Tallis", "Motet", 2, 1),
    ];
    const MOVEMENTS: &[&str] = &[
        "I. Allegro con brio",
        "II. Adagio",
        "III. Menuetto",
        "IV. Presto",
    ];
    const MIDNIGHT: i64 = 1_790_035_200; // a UTC midnight

    fn shelf_items() -> Vec<LibraryItem> {
        let mut items = Vec::new();
        for (ci, &(composer, form, works, movements)) in SHELF.iter().enumerate() {
            let surname = composer.rsplit(' ').next().unwrap();
            for w in 1..=works {
                for (m, movement) in MOVEMENTS.iter().take(movements).enumerate() {
                    let title = if movements == 1 {
                        format!("{form} No. {w}")
                    } else {
                        format!("{form} No. {w} in C Major, Op. {w}: {movement}")
                    };
                    let n = items.len();
                    items.push(LibraryItem {
                        source_uri: format!("spotify:track:{ci}-{w}-{m}"),
                        title,
                        artists: vec![composer.into(), format!("Test Ensemble {ci}")],
                        album: Some(format!("{surname}: {form}s, Vol. {}", w / 5)),
                        album_uri: Some(format!("spotify:album:{ci}-{}", w / 5)),
                        album_artists: vec![format!("Test Ensemble {ci}")],
                        disc_number: None,
                        track_number: None,
                        added_at: None,
                        genres: Vec::new(),
                        label: None,
                        duration_secs: Some(if movements == 1 { 240 } else { 420 }),
                        explicit: false,
                        origin: if n % 7 == 0 {
                            Origin::LikedTrack
                        } else {
                            Origin::SavedAlbum
                        },
                    });
                }
            }
        }
        for (i, (title, artist)) in [("Shape of You", "Ed Sheeran"), ("Mambo No. 5", "Lou Bega")]
            .into_iter()
            .enumerate()
        {
            items.push(LibraryItem {
                source_uri: format!("spotify:track:pop-{i}"),
                title: title.into(),
                artists: vec![artist.into()],
                album: None,
                album_uri: None,
                album_artists: Vec::new(),
                disc_number: None,
                track_number: None,
                added_at: None,
                genres: Vec::new(),
                label: None,
                duration_secs: Some(230),
                explicit: false,
                origin: Origin::LikedTrack,
            });
        }
        items
    }

    fn shelf() -> CandidatePool {
        CandidatePool::build(&shelf_items())
    }

    /// Simulate continuous listening from midnight. With `hour` fixed the
    /// time-of-day target stays put; otherwise it follows the clock.
    fn simulate<'p>(
        pool: &'p CandidatePool,
        config: &DjConfig,
        seed: u64,
        count: usize,
        hour: Option<u8>,
    ) -> Vec<&'p ClassicalTrack> {
        let mut rng = Rng::new(seed);
        let mut history = Vec::new();
        let mut picks = Vec::new();
        let mut now = MIDNIGHT;
        for _ in 0..count {
            let clock_hour = u8::try_from((now - MIDNIGHT) / 3600 % 24).unwrap();
            let ctx = PickContext {
                history: &history,
                now: Some(now),
                local_hour: Some(hour.unwrap_or(clock_hour)),
                energy_target: None,
            };
            let t = pick_next(pool, &ctx, config, &mut rng).unwrap();
            history.push(PlayRecord::at(t.track.source_uri.clone(), now));
            now += i64::from(t.track.duration_secs.unwrap());
            picks.push(t);
        }
        picks
    }

    /// The pre-refinement selector: uniform among tracks not in the last
    /// `avoid` plays. The baseline the DJ's variety must beat.
    fn uniform(
        pool: &CandidatePool,
        seed: u64,
        count: usize,
        avoid: usize,
    ) -> Vec<&ClassicalTrack> {
        let mut rng = Rng::new(seed);
        let mut picks: Vec<&ClassicalTrack> = Vec::new();
        for _ in 0..count {
            let recent: HashSet<&str> = picks
                .iter()
                .rev()
                .take(avoid)
                .map(|t| t.track.source_uri.as_str())
                .collect();
            let fresh: Vec<&ClassicalTrack> = pool
                .tracks()
                .iter()
                .filter(|t| !recent.contains(t.track.source_uri.as_str()))
                .collect();
            picks.push(fresh[rng.below(fresh.len())]);
        }
        picks
    }

    fn repeat_rate<K: PartialEq>(
        picks: &[&ClassicalTrack],
        key: impl Fn(&ClassicalTrack) -> K,
    ) -> f64 {
        let repeats = picks.windows(2).filter(|w| key(w[0]) == key(w[1])).count();
        repeats as f64 / (picks.len() - 1) as f64
    }

    fn longest_run<K: PartialEq>(
        picks: &[&ClassicalTrack],
        key: impl Fn(&ClassicalTrack) -> K,
    ) -> usize {
        let (mut best, mut run) = (1, 1);
        for w in picks.windows(2) {
            run = if key(w[0]) == key(w[1]) { run + 1 } else { 1 };
            best = best.max(run);
        }
        best
    }

    fn share(picks: &[&ClassicalTrack], composer: &str) -> f64 {
        picks.iter().filter(|t| t.composer == composer).count() as f64 / picks.len() as f64
    }

    /// Mean number of distinct keys per sliding window: local spread, which
    /// is what a listener hears (over a long run the hard cooldown makes any
    /// selector cycle the whole pool, so totals converge to the shelf mix).
    fn distinct_per_window<K: Ord>(
        picks: &[&ClassicalTrack],
        size: usize,
        key: impl Fn(&ClassicalTrack) -> K,
    ) -> f64 {
        let windows = picks.windows(size);
        let n = windows.len() as f64;
        windows
            .map(|w| w.iter().map(|t| key(t)).collect::<BTreeSet<_>>().len() as f64)
            .sum::<f64>()
            / n
    }

    fn min_track_gap(picks: &[&ClassicalTrack]) -> usize {
        let mut last: HashMap<&str, usize> = HashMap::new();
        let mut gap = usize::MAX;
        for (i, t) in picks.iter().enumerate() {
            if let Some(prev) = last.insert(t.track.source_uri.as_str(), i) {
                gap = gap.min(i - prev);
            }
        }
        gap
    }

    fn mean_energy(picks: &[&ClassicalTrack]) -> f64 {
        picks.iter().map(|t| f64::from(t.energy)).sum::<f64>() / picks.len() as f64
    }

    fn mean_energy_step(picks: &[&ClassicalTrack]) -> f64 {
        let steps: u32 = picks
            .windows(2)
            .map(|w| u32::from(w[0].energy.abs_diff(w[1].energy)))
            .sum();
        f64::from(steps) / (picks.len() - 1) as f64
    }

    fn uris(picks: &[&ClassicalTrack]) -> Vec<String> {
        picks.iter().map(|t| t.track.source_uri.clone()).collect()
    }

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
    fn shelf_pool_is_classical_only_and_fully_attributed() {
        let pool = shelf();
        assert_eq!(pool.len(), 291);
        assert!(
            pool.tracks()
                .iter()
                .all(|t| t.known_composer && t.period != Period::Unknown)
        );
        assert!(pool.get("spotify:track:pop-0").is_none());
    }

    #[test]
    fn empty_pool_yields_nothing() {
        let pool = CandidatePool::default();
        let (ctx, config) = (PickContext::default(), DjConfig::default());
        let mut rng = Rng::new(1);
        assert!(pick_next(&pool, &ctx, &config, &mut rng).is_none());
        assert_eq!(plan(&pool, &ctx, &config, 5, &mut rng).len(), 0);
    }

    #[test]
    fn long_run_spreads_composers_works_and_periods() {
        let pool = shelf();
        let config = DjConfig::default();
        for seed in [42, 7, 2026] {
            let dj = simulate(&pool, &config, seed, 1000, None);
            let base = uniform(&pool, seed, 1000, 5);
            let composer = |t: &ClassicalTrack| t.composer_key.clone();
            let bach = "Johann Sebastian Bach";
            let (dj_rep, base_rep) = (repeat_rate(&dj, composer), repeat_rate(&base, composer));
            let (dj_bach, base_bach) = (share(&dj, bach), share(&base, bach));
            let (dj_periods, base_periods) = (
                distinct_per_window(&dj, 8, |t| t.period),
                distinct_per_window(&base, 8, |t| t.period),
            );
            let (dj_composers, base_composers) = (
                distinct_per_window(&dj, 8, composer),
                distinct_per_window(&base, 8, composer),
            );
            eprintln!(
                "seed {seed}: composer repeat {dj_rep:.3} (uniform {base_rep:.3}), \
                 Bach share {dj_bach:.3} (uniform {base_bach:.3}), \
                 distinct composers/8 {dj_composers:.2} (uniform {base_composers:.2}), \
                 distinct periods/8 {dj_periods:.2} (uniform {base_periods:.2}), \
                 longest period run {}, min track gap {}",
                longest_run(&dj, |t| t.period),
                min_track_gap(&dj),
            );

            // Anti-repeat: the hard cooldown holds across the whole run.
            assert!(min_track_gap(&dj) >= config.track_cooldown_plays);

            // Composers: back-to-back repeats are rare and far below uniform.
            assert!(dj_rep <= 0.02, "composer repeat rate {dj_rep}");
            assert!(dj_rep * 4.0 < base_rep, "dj {dj_rep} vs uniform {base_rep}");
            assert!(longest_run(&dj, composer) <= 2);

            // Works and albums: never back to back.
            assert_eq!(repeat_rate(&dj, |t| t.work_key.clone()), 0.0);
            assert!(repeat_rate(&dj, |t| t.album_key.clone()) <= 0.01);

            // Every composer gets airtime; the prolific one is damped below
            // its shelf share while uniform tracks it.
            let heard: HashSet<&str> = dj.iter().map(|t| t.composer.as_str()).collect();
            assert_eq!(heard.len(), SHELF.len(), "unheard composers");
            let bach_shelf = 80.0 / 291.0;
            assert!(
                dj_bach < 0.75 * bach_shelf,
                "Bach share {dj_bach} vs shelf {bach_shelf}"
            );
            assert!(
                base_bach > 0.85 * bach_shelf,
                "uniform Bach share {base_bach}"
            );

            // Any eight consecutive tracks: nearly eight composers, and more
            // eras than uniform manages; never more than three of one era in
            // a row.
            assert!(
                dj_composers >= 7.0,
                "distinct composers per 8: {dj_composers}"
            );
            assert!(dj_composers > base_composers + 1.0);
            assert!(
                dj_periods > base_periods + 0.5,
                "periods per 8: {dj_periods} vs {base_periods}"
            );
            assert!(longest_run(&dj, |t| t.period) <= 3);
        }
    }

    #[test]
    fn energy_follows_the_time_of_day() {
        let pool = shelf();
        let config = DjConfig::default();
        // An evening's worth of picks each (a longer run exhausts the mood's
        // share of a 291-track shelf under the 24 h cooldown).
        let night = simulate(&pool, &config, 9, 100, Some(23));
        let morning = simulate(&pool, &config, 9, 100, Some(10));
        let base = uniform(&pool, 9, 100, 5);
        let miss = |picks: &[&ClassicalTrack], hour| {
            let target = energy_target_for_hour(hour);
            picks
                .iter()
                .map(|t| f64::from(t.energy.abs_diff(target)))
                .sum::<f64>()
                / picks.len() as f64
        };
        eprintln!(
            "mean energy: night {:.1}, morning {:.1}; mean miss from target: night {:.1} \
             (uniform {:.1}), morning {:.1} (uniform {:.1})",
            mean_energy(&night),
            mean_energy(&morning),
            miss(&night, 23),
            miss(&base, 23),
            miss(&morning, 10),
            miss(&base, 10),
        );
        assert!(mean_energy(&night) + 12.0 < mean_energy(&morning));
        assert!(miss(&night, 23) < 0.6 * miss(&base, 23));
        assert!(miss(&morning, 10) < 0.8 * miss(&base, 10));

        // An explicit target overrides the clock.
        let mut rng = Rng::new(5);
        let calm = PickContext {
            local_hour: Some(10),
            energy_target: Some(15),
            ..PickContext::default()
        };
        let picks = plan(&pool, &calm, &config, 100, &mut rng);
        assert!(
            mean_energy(&picks) < 40.0,
            "override mean {}",
            mean_energy(&picks)
        );
    }

    #[test]
    fn transitions_are_smoother_than_uniform() {
        let pool = shelf();
        let dj = simulate(&pool, &DjConfig::default(), 11, 800, Some(14));
        let base = uniform(&pool, 11, 800, 5);
        let (dj_step, base_step) = (mean_energy_step(&dj), mean_energy_step(&base));
        assert!(
            dj_step + 5.0 < base_step,
            "dj step {dj_step} vs uniform {base_step}"
        );
        let jarring = |p: &[&ClassicalTrack]| {
            p.windows(2)
                .filter(|w| w[0].energy.abs_diff(w[1].energy) > 60)
                .count()
        };
        assert!(
            jarring(&dj) * 3 < jarring(&base),
            "{} vs {}",
            jarring(&dj),
            jarring(&base)
        );
    }

    #[test]
    fn same_seed_same_set() {
        let pool = shelf();
        let config = DjConfig::default();
        let a = simulate(&pool, &config, 1234, 200, None);
        let b = simulate(&pool, &config, 1234, 200, None);
        let c = simulate(&pool, &config, 1235, 200, None);
        assert_eq!(uris(&a), uris(&b));
        assert_ne!(uris(&a), uris(&c));
    }

    #[test]
    fn plan_continues_history_without_repeats() {
        let pool = shelf();
        let config = DjConfig::default();
        let mut rng = Rng::new(77);
        let first = plan(&pool, &PickContext::default(), &config, 40, &mut rng);
        let history: Vec<PlayRecord> = first
            .iter()
            .map(|t| PlayRecord::new(t.track.source_uri.clone()))
            .collect();
        let ctx = PickContext {
            history: &history,
            ..PickContext::default()
        };
        let next = plan(&pool, &ctx, &config, 40, &mut rng);
        let all: Vec<&ClassicalTrack> = first.into_iter().chain(next).collect();
        assert_eq!(
            all.iter()
                .map(|t| &t.track.source_uri)
                .collect::<HashSet<_>>()
                .len(),
            80
        );
        assert_eq!(repeat_rate(&all, |t| t.work_key.clone()), 0.0);
    }

    fn tiny_pool(composers: &[&str]) -> CandidatePool {
        let items: Vec<LibraryItem> = composers
            .iter()
            .enumerate()
            .map(|(i, composer)| LibraryItem {
                source_uri: format!("spotify:track:tiny-{i}"),
                title: format!("Sonata No. {i}: I. Allegro"),
                artists: vec![(*composer).into()],
                album: None,
                album_uri: None,
                album_artists: Vec::new(),
                disc_number: None,
                track_number: None,
                added_at: None,
                genres: Vec::new(),
                label: None,
                duration_secs: Some(300),
                explicit: false,
                origin: Origin::SavedAlbum,
            })
            .collect();
        CandidatePool::build(&items)
    }

    #[test]
    fn small_pool_rotates_instead_of_repeating() {
        let pool = tiny_pool(&["Joseph Haydn", "Franz Schubert", "Claude Debussy"]);
        assert_eq!(pool.len(), 3);
        let picks = simulate(&pool, &DjConfig::default(), 3, 30, Some(12));
        assert_eq!(repeat_rate(&picks, |t| t.track.source_uri.clone()), 0.0);
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for t in &picks {
            *counts.entry(t.track.source_uri.as_str()).or_default() += 1;
        }
        assert!(counts.values().all(|&n| n == 10), "{counts:?}");

        let solo = tiny_pool(&["Erik Satie"]);
        assert!(
            simulate(&solo, &DjConfig::default(), 3, 5, None)
                .iter()
                .all(|t| t.composer == "Erik Satie")
        );
    }

    #[test]
    fn time_cooldown_excludes_recent_plays() {
        let pool = tiny_pool(&["Joseph Haydn", "Franz Schubert", "Claude Debussy"]);
        let config = DjConfig {
            track_cooldown_plays: 0,
            ..DjConfig::default()
        };
        let now = MIDNIGHT + 20 * 3600;
        let favorite = "spotify:track:tiny-0";
        let picked_ever = |played_at: i64| {
            // Forty manual plays since, so only the clock can hold it back.
            let mut history = vec![PlayRecord::at(favorite, played_at)];
            history.extend((0..40).map(|i| PlayRecord::new(format!("spotify:track:manual-{i}"))));
            (0..400).any(|seed| {
                let ctx = PickContext {
                    history: &history,
                    now: Some(now),
                    ..PickContext::default()
                };
                pick_next(&pool, &ctx, &config, &mut Rng::new(seed))
                    .unwrap()
                    .track
                    .source_uri
                    == favorite
            })
        };
        assert!(!picked_ever(now - 3600), "played an hour ago");
        assert!(picked_ever(now - 2 * 24 * 3600), "played two days ago");
    }

    #[test]
    fn plays_outside_the_pool_are_tolerated() {
        let pool = shelf();
        let history: Vec<PlayRecord> = (0..50)
            .map(|i| PlayRecord::new(format!("spotify:track:manual-{i}")))
            .collect();
        let ctx = PickContext {
            history: &history,
            ..PickContext::default()
        };
        let picks = plan(&pool, &ctx, &DjConfig::default(), 10, &mut Rng::new(2));
        assert_eq!(picks.len(), 10);
    }

    #[test]
    fn liked_tracks_are_favored() {
        let mut items: Vec<LibraryItem> = Vec::new();
        for (i, origin) in [Origin::LikedTrack, Origin::SavedAlbum]
            .into_iter()
            .enumerate()
        {
            items.push(LibraryItem {
                source_uri: format!("spotify:track:twin-{i}"),
                title: format!("Nocturne No. {i}"),
                artists: vec!["Frédéric Chopin".into()],
                album: None,
                album_uri: None,
                album_artists: Vec::new(),
                disc_number: None,
                track_number: None,
                added_at: None,
                genres: Vec::new(),
                label: None,
                duration_secs: Some(300),
                explicit: false,
                origin,
            });
        }
        let pool = CandidatePool::build(&items);
        let mut rng = Rng::new(8);
        let liked = (0..4000)
            .filter(|_| {
                pick_next(
                    &pool,
                    &PickContext::default(),
                    &DjConfig::default(),
                    &mut rng,
                )
                .unwrap()
                .origin
                .is_liked()
            })
            .count();
        // Expected 1.3 : 1 → ~56.5% liked.
        assert!((2140..2380).contains(&liked), "liked picked {liked}/4000");
    }

    #[test]
    fn weight_curves() {
        assert_eq!(spacing_pm(None, 5), 1000);
        assert_eq!(spacing_pm(Some(5), 5), 1000);
        assert_eq!(spacing_pm(Some(0), 5), 27);
        assert_eq!(spacing_pm(Some(4), 5), 694);
        assert_eq!(spacing_pm(Some(0), 30), 1);
        assert_eq!(spacing_pm(Some(3), 0), 1000);
        assert_eq!(energy_fit_pm(50, 50), 1000);
        assert_eq!(energy_fit_pm(70, 50), 800);
        assert_eq!(energy_fit_pm(90, 10), 80);
        assert_eq!(energy_jump_pm(20, 55), 1000);
        assert_eq!(energy_jump_pm(10, 60), 550);
        assert_eq!(energy_jump_pm(5, 95), 150);
        assert_eq!(staleness_pm(0, 150), 500);
        assert_eq!(staleness_pm(450, 150), 1000);
        assert_eq!(staleness_pm(9, 0), 1000);
        for h in 0..24 {
            assert!(energy_target_for_hour(h) <= 60);
        }
        assert!(energy_target_for_hour(2) < energy_target_for_hour(10));
    }
}
