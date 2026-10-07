//! A shared, synthetic classical shelf and simulation helpers for the DJ's
//! seeded tests (dj and steer). Every identifier is made up.
#![allow(clippy::cast_precision_loss)]

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;

use crate::classical::CandidatePool;
use crate::dj::{DjConfig, PickContext, PlannedWork, PlayRecord, Rng, WorkPool, pick_next};
use crate::feedback::FeedbackModel;
use crate::library::{LibraryItem, Origin};
use crate::steer::Steer;
use crate::works::Work;

/// A realistic, lopsided shelf: (composer, form, works, movements/work).
pub(crate) const SHELF: &[(&str, &str, usize, usize)] = &[
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
/// First movements rotate so works differ in energy (82, 47, 40, 20).
pub(crate) const FIRSTS: &[&str] = &[
    "I. Allegro con brio",
    "I. Adagio - Allegro",
    "I. Andante",
    "I. Largo",
];
pub(crate) const REST: &[&str] = &["II. Adagio", "III. Menuetto", "IV. Presto"];
pub(crate) const MIDNIGHT: i64 = 1_790_035_200; // a UTC midnight

pub(crate) fn item(
    uri: String,
    title: String,
    composer: &str,
    album: (String, String),
    secs: u32,
    origin: Origin,
) -> LibraryItem {
    LibraryItem {
        source_uri: uri,
        title,
        artists: vec![composer.into(), "Test Ensemble".into()],
        album: Some(album.0),
        album_uri: Some(album.1),
        album_artists: vec!["Test Ensemble".into()],
        disc_number: None,
        track_number: None,
        added_at: None,
        genres: Vec::new(),
        label: None,
        duration_secs: Some(secs),
        explicit: false,
        origin,
    }
}

/// The shelf, with `scale` times as many works per composer.
pub(crate) fn shelf_items(scale: usize) -> Vec<LibraryItem> {
    let mut items = Vec::new();
    for (ci, &(composer, form, works, movements)) in SHELF.iter().enumerate() {
        let surname = composer.rsplit(' ').next().unwrap();
        for w in 1..=works * scale {
            let album = (
                format!("{surname}: {form}s, Vol. {}", w / 5),
                format!("spotify:album:{ci}-{}", w / 5),
            );
            let origin = if (ci + w) % 7 == 0 {
                Origin::LikedTrack
            } else {
                Origin::SavedAlbum
            };
            if movements == 1 {
                let uri = format!("spotify:track:{ci}-{w}-0");
                items.push(item(
                    uri,
                    format!("{form} No. {w}"),
                    composer,
                    album,
                    240,
                    origin,
                ));
                continue;
            }
            let titles = std::iter::once(FIRSTS[w % FIRSTS.len()])
                .chain(REST.iter().copied().take(movements - 1));
            for (m, movement) in titles.enumerate() {
                let uri = format!("spotify:track:{ci}-{w}-{m}");
                let title = format!("{form} No. {w} in C Major, Op. {w}: {movement}");
                items.push(item(uri, title, composer, album.clone(), 420, origin));
            }
        }
    }
    items
}

/// A two-hour opera: 24 scenes.
pub(crate) fn opera() -> Vec<LibraryItem> {
    (1..=24)
        .map(|n| {
            let album = (
                "Verdi: La traviata".to_owned(),
                "spotify:album:traviata".to_owned(),
            );
            let title = format!("La traviata, Act {}: Scene {n}", 1 + n / 9);
            let uri = format!("spotify:track:traviata-{n}");
            item(uri, title, "Giuseppe Verdi", album, 300, Origin::SavedAlbum)
        })
        .collect()
}

pub(crate) fn works_of(items: &[LibraryItem]) -> WorkPool {
    WorkPool::new(&CandidatePool::build(items))
}

/// Continuous listening from midnight: each pick's movements play back to
/// back. With `hour` fixed the energy target stays put.
pub(crate) fn simulate<'p>(
    pool: &'p WorkPool,
    config: &DjConfig,
    seed: u64,
    count: usize,
    hour: Option<u8>,
) -> Vec<PlannedWork<'p>> {
    simulate_steered(pool, config, seed, count, hour, None)
}

/// [`simulate`], steered.
pub(crate) fn simulate_steered<'p>(
    pool: &'p WorkPool,
    config: &DjConfig,
    seed: u64,
    count: usize,
    hour: Option<u8>,
    steer: Option<&Steer>,
) -> Vec<PlannedWork<'p>> {
    simulate_with(pool, config, seed, count, hour, steer, None)
}

/// [`simulate`], steered and with the owner's feedback.
pub(crate) fn simulate_with<'p>(
    pool: &'p WorkPool,
    config: &DjConfig,
    seed: u64,
    count: usize,
    hour: Option<u8>,
    steer: Option<&Steer>,
    feedback: Option<&FeedbackModel>,
) -> Vec<PlannedWork<'p>> {
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
            steer,
            feedback,
        };
        let planned = pick_next(pool, &ctx, config, &mut rng).unwrap();
        for m in planned.movements {
            history.push(PlayRecord::at(m.track.source_uri.clone(), now));
            now += i64::from(m.track.duration_secs.unwrap());
        }
        picks.push(planned);
    }
    picks
}

/// Uniform among works not in the last `avoid` picks: the baseline.
pub(crate) fn uniform(pool: &WorkPool, seed: u64, count: usize, avoid: usize) -> Vec<&Work> {
    let mut rng = Rng::new(seed);
    let mut picks: Vec<&Work> = Vec::new();
    for _ in 0..count {
        let fresh: Vec<&Work> = pool
            .works()
            .iter()
            .filter(|w| !picks.iter().rev().take(avoid).any(|r| std::ptr::eq(*r, *w)))
            .collect();
        picks.push(fresh[rng.below(fresh.len())]);
    }
    picks
}

pub(crate) fn works<'p>(picks: &[PlannedWork<'p>]) -> Vec<&'p Work> {
    picks.iter().map(|p| p.work).collect()
}

pub(crate) fn repeat_rate<K: PartialEq>(picks: &[&Work], key: impl Fn(&Work) -> K) -> f64 {
    let repeats = picks.windows(2).filter(|w| key(w[0]) == key(w[1])).count();
    repeats as f64 / (picks.len() - 1) as f64
}

pub(crate) fn longest_run<K: PartialEq>(picks: &[&Work], key: impl Fn(&Work) -> K) -> usize {
    let (mut best, mut run) = (1, 1);
    for w in picks.windows(2) {
        run = if key(w[0]) == key(w[1]) { run + 1 } else { 1 };
        best = best.max(run);
    }
    best
}

pub(crate) fn distinct_per_window<K: Ord>(
    picks: &[&Work],
    size: usize,
    key: impl Fn(&Work) -> K,
) -> f64 {
    let windows = picks.windows(size);
    let n = windows.len() as f64;
    windows
        .map(|w| w.iter().map(|t| key(t)).collect::<BTreeSet<_>>().len() as f64)
        .sum::<f64>()
        / n
}

pub(crate) fn min_work_gap(picks: &[&Work]) -> usize {
    let mut last: HashMap<*const Work, usize> = HashMap::new();
    let mut gap = usize::MAX;
    for (i, w) in picks.iter().enumerate() {
        if let Some(prev) = last.insert(std::ptr::from_ref(*w), i) {
            gap = gap.min(i - prev);
        }
    }
    gap
}

pub(crate) fn mean_energy(picks: &[&Work]) -> f64 {
    picks.iter().map(|w| f64::from(w.energy())).sum::<f64>() / picks.len() as f64
}

/// Everything needed to reproduce a failing seeded run from its log alone:
/// the seed, the pool size, and the planned sequence with each pick's
/// composer, period, energy and reason.
pub(crate) fn transcript(seed: u64, pool: &WorkPool, picks: &[PlannedWork<'_>]) -> String {
    let mut out = format!(
        "seed {seed}, pool {} works, {} picks:\n",
        pool.len(),
        picks.len()
    );
    for (i, p) in picks.iter().enumerate() {
        let _ = writeln!(
            out,
            "{i:>4}  {} | {} | {:?} | energy {} | {}",
            p.work.title,
            p.work.composer,
            p.work.period,
            p.work.energy(),
            p.reason.summary
        );
    }
    out
}
