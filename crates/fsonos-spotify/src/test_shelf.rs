//! A shared, synthetic classical shelf, a shelf of songs in other genres,
//! and simulation helpers for the DJ's seeded tests (dj, steer, feedback and
//! the feed). Every identifier is made up.
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
        artist_id: None,
        album: Some(album.0),
        album_uri: Some(album.1),
        album_artists: vec!["Test Ensemble".into()],
        disc_number: None,
        track_number: None,
        added_at: None,
        genres: Vec::new(),
        release_year: None,
        taste_pm: None,
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

/// Albums of songs, no classical music: (artist, album, genre, titles). The
/// titles test what a song must not be mistaken for: soundtrack cues and
/// interludes titled `Work: Part`, a deluxe edition's acoustic take of a song
/// it also holds, and words from the classical tempo table ("Lullaby").
pub(crate) const SONGS: &[(&str, &str, &str, &[&str])] = &[
    (
        "Juniper Vale",
        "Paper Moons",
        "pop",
        &[
            "Paper Moons",
            "Glasshouse",
            "Lullaby for a Fast Car",
            "Summer Static",
            "Neon Harbor",
        ],
    ),
    (
        "The Lantern Club",
        "Harbor Lights (Deluxe)",
        "indie pop",
        &[
            "Harbor Lights",
            "Every Window",
            "Low Tide",
            "Harbor Lights - Acoustic Version",
        ],
    ),
    (
        "Nina Marsh Quartet",
        "Blue Hours",
        "jazz",
        &[
            "Blue Hours",
            "Slow Pier",
            "Take the Long Way",
            "Ferry at Midnight",
        ],
    ),
    (
        "Otis Fairweather Trio",
        "Late Set",
        "jazz",
        &["Late Set", "Coffee and Rain", "Walking Bass Blues"],
    ),
    (
        "MC Halcyon",
        "Rooftop Theory",
        "hip hop",
        &[
            "Intro",
            "Skyline",
            "Interlude: Night Drive",
            "Block Party",
            "Interlude: Day Shift",
        ],
    ),
    (
        "Ada Brightwell",
        "Starfall (Original Soundtrack)",
        "soundtrack",
        &[
            "Starfall: Main Title",
            "Starfall: The Chase",
            "Starfall: Homecoming",
        ],
    ),
];

/// The release year of each [`SONGS`] album.
pub(crate) const SONG_YEARS: [u16; 6] = [2019, 2012, 1962, 1958, 1997, 2021];

/// [`SONGS`] as library items: saved albums with one more song of Juniper
/// Vale's liked on its own, and an explicit track of MC Halcyon's (in the
/// pool, flagged: it plays only if the owner's preferences allow).
pub(crate) fn song_items() -> Vec<LibraryItem> {
    let mut items = Vec::new();
    for (a, &(artist, album, genre, titles)) in SONGS.iter().enumerate() {
        for (t, title) in (1u32..).zip(titles.iter()) {
            items.push(LibraryItem {
                source_uri: format!("spotify:track:song-{a}-{t}"),
                title: (*title).to_owned(),
                artists: vec![artist.into()],
                artist_id: None,
                album: Some(album.into()),
                album_uri: Some(format!("spotify:album:songs-{a}")),
                album_artists: vec![artist.into()],
                disc_number: Some(1),
                track_number: Some(t),
                added_at: None,
                genres: vec![genre.into()],
                release_year: Some(SONG_YEARS[a]),
                taste_pm: None,
                label: None,
                duration_secs: Some(200),
                explicit: false,
                origin: Origin::SavedAlbum,
            });
        }
    }
    let mut single = items[0].clone();
    single.source_uri = "spotify:track:song-single".into();
    single.title = "Kite Season (feat. Otis Fairweather)".into();
    single.artists.push("Otis Fairweather".into());
    single.album = Some("Kite Season".into());
    single.album_uri = Some("spotify:album:songs-single".into());
    single.release_year = Some(2023);
    single.track_number = Some(1);
    single.origin = Origin::LikedTrack;
    items.push(single);
    let mut explicit = items
        .iter()
        .find(|i| i.artists[0] == "MC Halcyon")
        .cloned()
        .expect("MC Halcyon's album");
    explicit.source_uri = "spotify:track:song-explicit".into();
    explicit.title = "Back Block".into();
    explicit.track_number = Some(9);
    explicit.explicit = true;
    items.push(explicit);
    items
}

/// A mixed-genre library: [`song_items`] plus the shelf's Beethoven,
/// Chopin and Pärt (multi-movement symphonies among them).
pub(crate) fn mixed_items() -> Vec<LibraryItem> {
    let mut items = song_items();
    items.extend(shelf_items(1).into_iter().filter(|i| {
        ["Ludwig van Beethoven", "Frédéric Chopin", "Arvo Pärt"].contains(&i.artists[0].as_str())
    }));
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
