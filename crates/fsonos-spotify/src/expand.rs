//! Completing works the library holds only part of.
//!
//! Liked Songs often holds a single movement — say, the Adagietto. In a
//! work-based DJ that movement would play alone, which is jarring; the album
//! knows the rest. [`complete_works`] reads each partly-held work's album track
//! list (`GET /v1/albums/{id}/tracks` with the owner's own token: a read of
//! public catalog data, cached in the store's album-track table so each album
//! is fetched once) and [`expand_work`] fills in the missing movements — from
//! the same recording, when an album holds more than one.
//!
//! Filled-in movements are not library items: they carry
//! [`ClassicalTrack::expanded`] and never count as liked, while the movement
//! the owner liked keeps its liked origin. Read-only — nothing is ever saved
//! to or removed from the owner's library. A work whose album can't be read
//! plays what the library has (at `DjConfig::incomplete_pm`); an album that
//! isn't there or has nothing playable is left alone for a week
//! ([`AlbumMisses`]), and a rate limit or outage stops the run (the rest is
//! tried next time) rather than waiting it out album by album.

use std::collections::{HashMap, HashSet};

use asupersync::Cx;
use fsonos_core::store::{AlbumTrack, Store};
use fsonos_types::Track;

use crate::SpotifyError;
use crate::classical::{ClassicalTrack, estimate_energy, normalize, split_title};
use crate::client::{Paging, SimplifiedTrack, renderable, secs};
use crate::dj::DjConfig;
use crate::library::Origin;
use crate::session::Session;
use crate::works::{Work, build, movement_number};

/// How long an album whose track list was missing or unplayable is left
/// alone before it is read again, in seconds (a week).
pub const MISS_RETRY_SECS: i64 = 7 * 86_400;

/// What [`complete_works`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Expansion {
    /// Works given movements from their album's track list.
    pub completed: usize,
    /// Album lists read from the Web API (the others came from the cache).
    pub fetched: usize,
    /// Albums whose list couldn't be read or cached, and why. Their works
    /// play as held — except that a list read but not cached still completes
    /// its works this run.
    pub failed: Vec<(String, String)>,
    /// Albums not asked for because a recent read found nothing there.
    pub skipped: usize,
    /// Why the run stopped early (rate limited past the waits, a server
    /// error, signed out, offline): the remaining works play as held and
    /// their albums are tried next run.
    pub halted: Option<String>,
}

impl Expansion {
    fn fail(&mut self, album_uri: &str, why: impl std::fmt::Display) {
        self.failed.push((album_uri.to_owned(), why.to_string()));
    }
}

/// Albums whose track list couldn't be had — not found, or nothing in it
/// playable — and when each may be tried again. Keep one across runs (the
/// daemon does) so such an album isn't fetched on every sync.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AlbumMisses {
    retry_at: HashMap<String, i64>,
}

impl AlbumMisses {
    fn waiting(&self, album_uri: &str, now: i64) -> bool {
        self.retry_at.get(album_uri).is_some_and(|&at| now < at)
    }

    fn record(&mut self, album_uri: &str, now: i64) {
        self.retry_at
            .insert(album_uri.to_owned(), now + MISS_RETRY_SECS);
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.retry_at.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.retry_at.is_empty()
    }
}

/// `spotify:album:<id>` → `<id>`.
#[must_use]
pub fn album_id(album_uri: &str) -> Option<&str> {
    album_uri
        .strip_prefix("spotify:album:")
        .filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric()))
}

/// Complete every partly-held work whose album is known. Album lists come
/// from the store's cache, else the Web API (then cached). Works that can't
/// be completed come back as they were — no failure loses any — and with
/// `config.expand_partial_works` off, nothing is read or changed.
pub async fn complete_works<S: Store + ?Sized>(
    session: &mut Session,
    cx: &Cx,
    store: &mut S,
    works: Vec<Work>,
    config: &DjConfig,
    now: i64,
    misses: &mut AlbumMisses,
) -> (Vec<Work>, Expansion) {
    let mut report = Expansion::default();
    if !config.expand_partial_works {
        return (works, report);
    }
    let mut unreadable: HashSet<String> = HashSet::new();
    let mut done = Vec::with_capacity(works.len());
    for work in works {
        let Some(uri) = work
            .album_uri
            .clone()
            .filter(|_| work.needs_expansion() && report.halted.is_none())
        else {
            done.push(work);
            continue;
        };
        if unreadable.contains(&uri) {
            done.push(work);
            continue;
        }
        if misses.waiting(&uri, now) {
            report.skipped += 1;
            unreadable.insert(uri);
            done.push(work);
            continue;
        }
        let Some(tracks) = album_list(session, cx, store, &uri, now, misses, &mut report).await
        else {
            unreadable.insert(uri);
            done.push(work);
            continue;
        };
        match expand_work(&work, &tracks) {
            Some(full) => {
                report.completed += 1;
                done.push(full);
            }
            None => done.push(work),
        }
    }
    (done, report)
}

/// `album_uri`'s track list from the cache, else the Web API (then cached).
/// `None` when it can't be had this run, with why in `report` — and a miss
/// recorded when the album itself is the problem.
async fn album_list<S: Store + ?Sized>(
    session: &mut Session,
    cx: &Cx,
    store: &mut S,
    album_uri: &str,
    now: i64,
    misses: &mut AlbumMisses,
    report: &mut Expansion,
) -> Option<Vec<AlbumTrack>> {
    match store.album_tracks(album_uri) {
        Ok(Some(cached)) => return Some(cached.tracks),
        Ok(None) => {}
        Err(e) => {
            report.fail(album_uri, SpotifyError::from(e));
            return None;
        }
    }
    match fetch_album_tracks(session, cx, album_uri).await {
        Ok(tracks) if tracks.is_empty() => {
            report.fail(album_uri, "no playable tracks");
            misses.record(album_uri, now);
            None
        }
        Ok(tracks) => {
            report.fetched += 1;
            if let Err(e) = store.save_album_tracks(album_uri, &tracks, now) {
                report.fail(album_uri, SpotifyError::from(e));
            }
            Some(tracks)
        }
        Err(e) if halts(&e) => {
            report.halted = Some(e.to_string());
            None
        }
        Err(e) => {
            report.fail(album_uri, e);
            misses.record(album_uri, now);
            None
        }
    }
}

/// Whether a failure is the Web API's rather than one album's — rate
/// limited past the waits, a server error, signed out, offline — so asking
/// for the next album now would only fail (or wait) again.
pub(crate) fn halts(e: &SpotifyError) -> bool {
    match e {
        SpotifyError::Api { status, .. } => matches!(status, 401 | 403 | 429) || *status >= 500,
        SpotifyError::Decode(_) => false,
        _ => true,
    }
}

/// Complete works from the cached album lists only (no network): what the
/// daemon does at startup before a sync.
pub fn complete_from_cache<S: Store + ?Sized>(
    store: &S,
    works: Vec<Work>,
) -> Result<Vec<Work>, SpotifyError> {
    works
        .into_iter()
        .map(|work| {
            let cached = match work.album_uri.as_deref().filter(|_| work.needs_expansion()) {
                Some(uri) => store.album_tracks(uri)?,
                None => None,
            };
            Ok(cached
                .and_then(|c| expand_work(&work, &c.tracks))
                .unwrap_or(work))
        })
        .collect()
}

/// An album's whole track list from the Web API, every page, in album
/// order. Tracks Sonos can't render (unplayable in the owner's market) are
/// left out.
pub async fn fetch_album_tracks(
    session: &mut Session,
    cx: &Cx,
    album_uri: &str,
) -> Result<Vec<AlbumTrack>, SpotifyError> {
    let id = album_id(album_uri)
        .ok_or_else(|| SpotifyError::Decode(format!("not a Spotify album URI: {album_uri}")))?;
    let mut next = Some(session.endpoints().album_tracks(id, 0));
    let mut requested = HashSet::new();
    let mut tracks = Vec::new();
    while let Some(url) = next.take() {
        if !requested.insert(url.clone()) {
            return Err(SpotifyError::Decode(format!("paging loop at {url}")));
        }
        let page = Paging::<SimplifiedTrack>::parse(&session.get(cx, &url).await?)?;
        tracks.extend(page.items.iter().filter_map(album_track));
        next = page.next;
    }
    tracks.sort_by_key(|t| (t.disc_number, t.track_number));
    Ok(tracks)
}

/// One album-page track as an album-track cache row.
fn album_track(t: &SimplifiedTrack) -> Option<AlbumTrack> {
    renderable(&t.uri, t.is_local, t.is_playable).then(|| AlbumTrack {
        disc_number: t.disc_number.max(1),
        track_number: t.track_number,
        source_uri: t.uri.clone(),
        title: t.name.clone(),
        duration_secs: secs(t.duration_ms),
    })
}

/// `work` completed from its album's track list: every album track of the
/// same recording of the work joins it, in disc/track order. `None` when the
/// album adds nothing.
#[must_use]
pub fn expand_work(work: &Work, album: &[AlbumTrack]) -> Option<Work> {
    let template = work.movements.first()?;
    let added: Vec<ClassicalTrack> = recording(work, album)
        .into_iter()
        .filter(|t| !holds(work, t))
        .map(|t| filled_in(template, t))
        .collect();
    if added.is_empty() {
        return None;
    }
    let mut movements: Vec<&ClassicalTrack> = work.movements.iter().chain(&added).collect();
    movements.sort_by_key(|m| {
        (
            m.disc_number.unwrap_or(1),
            m.track_number.unwrap_or(u32::MAX),
        )
    });
    Some(build(&movements))
}

/// Whether `work` already holds album track `t`: the same track, or a
/// movement at the same disc and track position (a relinked copy).
fn holds(work: &Work, t: &AlbumTrack) -> bool {
    work.movements.iter().any(|m| {
        m.track.source_uri == t.source_uri
            || (m.disc_number, m.track_number) == (Some(t.disc_number), Some(t.track_number))
    })
}

/// The album tracks of `work`'s recording. The tracks titled as the work
/// split into recordings wherever the movement numbering starts over or a
/// movement repeats; with more than one, the recordings holding the work's
/// own movements are kept.
fn recording<'a>(work: &Work, album: &'a [AlbumTrack]) -> Vec<&'a AlbumTrack> {
    let title = normalize(&work.title);
    let mut runs: Vec<Vec<&AlbumTrack>> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut last: Option<u32> = None;
    for t in album {
        let parts = split_title(&t.title);
        if normalize(parts.work) != title {
            continue;
        }
        let movement = normalize(parts.movement.unwrap_or(&t.title));
        let number = parts.movement.and_then(movement_number);
        let restarts = matches!((last, number), (Some(prev), Some(n)) if n <= prev);
        if runs.is_empty() || restarts || seen.contains(&movement) {
            runs.push(Vec::new());
            seen.clear();
            last = None;
        }
        if let Some(run) = runs.last_mut() {
            run.push(t);
        }
        seen.insert(movement);
        last = number.or(last);
    }
    if runs.len() > 1 {
        runs.retain(|run| run.iter().any(|t| holds(work, t)));
    }
    runs.concat()
}

/// A missing movement, shaped like the work's held one: same composer,
/// work, album and performers; its own title, position and energy.
fn filled_in(template: &ClassicalTrack, t: &AlbumTrack) -> ClassicalTrack {
    let parts = split_title(&t.title);
    ClassicalTrack {
        track: Track {
            title: t.title.clone(),
            artist: template.track.artist.clone(),
            album: template.track.album.clone(),
            source_uri: t.source_uri.clone(),
            uri: None,
            duration_secs: t.duration_secs,
        },
        composer: template.composer.clone(),
        composer_key: template.composer_key.clone(),
        known_composer: template.known_composer,
        classical: template.classical,
        period: template.period,
        genres: template.genres.clone(),
        explicit: false,
        year: template.year,
        taste_pm: 1000,
        work: template.work.clone(),
        work_key: template.work_key.clone(),
        album_key: template.album_key.clone(),
        album_uri: template.album_uri.clone(),
        disc_number: Some(t.disc_number),
        track_number: Some(t.track_number),
        movement: parts.movement.map(str::to_owned),
        energy: estimate_energy(parts.movement, parts.work),
        // Neutral: from the album, not the owner's own saving or liking.
        origin: Origin::SavedAlbum,
        expanded: true,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use asupersync::http::Client;
    use fsonos_core::store::MemStore;

    use super::*;
    use crate::classical::CandidatePool;
    use crate::client::{CachedToken, SCOPE, TokenCache};
    use crate::fake_spotify::{FakeSpotify, config, runtime, scratch_dir};
    use crate::library::LibraryItem;
    use crate::works::{Completeness, group_works, movement_number};

    const ALBUM: &str = "spotify:album:FakeAlbum0000000000009";
    const ADAGIETTO: &str = "spotify:track:FakeMahlerTrack00000004";
    const NOW: i64 = 1_790_035_200;

    /// The one movement Liked Songs holds.
    fn liked(uri: &str, album_uri: &str) -> LibraryItem {
        LibraryItem {
            source_uri: uri.into(),
            title: "Symphony No. 5 in C-Sharp Minor: IV. Adagietto. Sehr langsam".into(),
            artists: vec!["Gustav Mahler".into(), "Test Philharmonic".into()],
            artist_id: None,
            album: Some("Mahler: Symphony No. 5".into()),
            album_uri: Some(album_uri.into()),
            album_artists: vec!["Gustav Mahler".into()],
            disc_number: Some(1),
            track_number: Some(4),
            added_at: Some(NOW),
            genres: Vec::new(),
            release_year: None,
            taste_pm: None,
            label: None,
            duration_secs: Some(604),
            explicit: false,
            origin: Origin::LikedTrack,
        }
    }

    fn works_of(items: &[LibraryItem]) -> Vec<Work> {
        group_works(CandidatePool::build(items).tracks())
    }

    fn fixture_album() -> Vec<AlbumTrack> {
        [
            include_bytes!("../tests/fixtures/album_tracks_page1.json").as_slice(),
            include_bytes!("../tests/fixtures/album_tracks_page2.json").as_slice(),
        ]
        .into_iter()
        .flat_map(|page| Paging::<SimplifiedTrack>::parse(page).unwrap().items)
        .filter_map(|t| album_track(&t))
        .collect()
    }

    #[test]
    fn one_liked_movement_becomes_the_whole_work() {
        let works = works_of(&[liked(ADAGIETTO, ALBUM)]);
        assert_eq!(works.len(), 1);
        let work = &works[0];
        // A lone "IV." is provably partial: the numbering starts late.
        assert_eq!(work.completeness, Completeness::Partial);
        let album = fixture_album();
        assert_eq!(album.len(), 6);

        let full = expand_work(work, &album).unwrap();
        let numerals: Vec<u32> = full
            .movements
            .iter()
            .filter_map(|m| m.movement.as_deref().and_then(movement_number))
            .collect();
        assert_eq!(
            numerals,
            [1, 2, 3, 4, 5],
            "in album order, the song left out"
        );
        assert_eq!(full.completeness, Completeness::Complete);
        assert_eq!(
            (full.work_key.as_str(), full.total_secs),
            (work.work_key.as_str(), 3015)
        );
        // The owner's movement stays liked; the filled-in ones never are.
        assert!(full.is_liked());
        for m in &full.movements {
            let held = m.track.source_uri == ADAGIETTO;
            assert_eq!(m.expanded, !held, "{}", m.track.title);
            assert_eq!(m.origin.is_liked(), held, "{}", m.track.title);
        }
        let finale = full.movements.last().unwrap();
        assert_eq!(finale.composer, "Gustav Mahler");
        assert_eq!(
            finale.track.artist.as_deref(),
            Some("Gustav Mahler; Test Philharmonic")
        );
        assert!(
            finale.energy > full.movements[3].energy,
            "Allegro giocoso over Adagietto"
        );
        assert!(expand_work(&full, &album).is_none(), "nothing left to add");

        assert_eq!(album_id(ALBUM), Some("FakeAlbum0000000000009"));
        for bad in [
            "spotify:track:FakeAlbum0000000000009",
            "spotify:album:",
            "spotify:album:a/b",
        ] {
            assert_eq!(album_id(bad), None, "{bad}");
        }
    }

    #[test]
    fn two_recordings_on_one_album_stay_apart() {
        // The album holds the symphony twice: tracks 1–5 and 6–10, then a song.
        let first: Vec<AlbumTrack> = fixture_album().into_iter().take(5).collect();
        let mut album = first.clone();
        for (number, t) in (6u32..).zip(&first) {
            album.push(AlbumTrack {
                track_number: number,
                source_uri: format!("spotify:track:FakeSecondTake{number:08}"),
                ..t.clone()
            });
        }
        album.extend(fixture_album().into_iter().skip(5));
        let uris = |work: &Work| -> Vec<String> {
            work.movements
                .iter()
                .map(|m| m.track.source_uri.clone())
                .collect()
        };

        // The owner liked the second take's Adagietto: the rest of that take.
        let mut second = liked("spotify:track:FakeSecondTake00000009", ALBUM);
        second.track_number = Some(9);
        let full = expand_work(&works_of(&[second])[0], &album).unwrap();
        let expected: Vec<String> = (6..=10)
            .map(|n| format!("spotify:track:FakeSecondTake{n:08}"))
            .collect();
        assert_eq!(uris(&full), expected);
        assert_eq!(full.completeness, Completeness::Complete);

        // The first take's, under another URI (relinked): found by position,
        // and not doubled.
        let relinked = liked("spotify:track:FakeRelinkedTrack00001", ALBUM);
        let full = expand_work(&works_of(&[relinked])[0], &album).unwrap();
        let mut expected: Vec<String> = first.iter().map(|t| t.source_uri.clone()).collect();
        expected[3] = "spotify:track:FakeRelinkedTrack00001".into();
        assert_eq!(uris(&full), expected);

        // Unnumbered movements split where one repeats.
        let suite = |n: u32, movement: &str| AlbumTrack {
            disc_number: 1,
            track_number: n,
            source_uri: format!("spotify:track:FakeSuite{n:013}"),
            title: format!("Suite in G: {movement}"),
            duration_secs: Some(120),
        };
        let album = [
            suite(1, "Prelude"),
            suite(2, "Gigue"),
            suite(3, "Prelude"),
            suite(4, "Gigue"),
        ];
        let mut gigue = liked(
            "spotify:track:FakeSuite0000000000004",
            "spotify:album:FakeSuiteAlbum0000001",
        );
        gigue.title = "Suite in G: Gigue".into();
        gigue.track_number = Some(4);
        let full = expand_work(&works_of(&[gigue])[0], &album).unwrap();
        assert_eq!(
            uris(&full),
            [
                "spotify:track:FakeSuite0000000000003",
                "spotify:track:FakeSuite0000000000004"
            ]
        );
    }

    /// A fake Spotify with a valid cached token, plus where that cache lives.
    fn authorized(name: &str) -> (FakeSpotify, TokenCache, PathBuf) {
        let spotify = FakeSpotify::start();
        {
            let mut fake = spotify.state.lock().unwrap();
            "access-1".clone_into(&mut fake.access);
            "refresh-1".clone_into(&mut fake.refresh);
            fake.rate_limit_albums = 1;
        }
        let data_dir = scratch_dir(name);
        let cache = TokenCache::in_data_dir(&data_dir);
        cache
            .store(&CachedToken {
                access_token: "access-1".into(),
                refresh_token: "refresh-1".into(),
                expires_at: i64::MAX / 2,
                scope: SCOPE.into(),
            })
            .unwrap();
        (spotify, cache, data_dir)
    }

    /// Requests the fake saw whose path contains `path`.
    fn requests(state: &Arc<std::sync::Mutex<crate::fake_spotify::Fake>>, path: &str) -> usize {
        let fake = state.lock().unwrap();
        fake.log.iter().filter(|l| l.contains(path)).count()
    }

    const MISSING: &str = "spotify:album:FakeAlbumMissing000000001";
    const UNPLAYABLE: &str = "spotify:album:FakeAlbumUnplayable001";

    /// Everything one run of the fetch scenario observed.
    struct Outcome {
        works: Vec<Work>,
        first: Expansion,
        first_took: Duration,
        cached: fsonos_core::store::CachedAlbum,
        second: Expansion,
        week_later: Expansion,
        offline: Vec<Work>,
        untouched: Vec<Work>,
        off: Expansion,
        /// Album requests made by the end of each run.
        asked: [usize; 4],
    }

    /// Complete a liked Adagietto (its album readable) and a liked movement
    /// whose album 404s: twice with the API, once a week later, once from the
    /// cache alone, and once with expansion switched off.
    fn fetch_scenario(spotify: &FakeSpotify, cache: TokenCache) -> Outcome {
        let endpoints = spotify.endpoints();
        let state = Arc::clone(&spotify.state);
        let items = [
            liked(ADAGIETTO, ALBUM),
            liked("spotify:track:FakeElsewhere0000000001", MISSING),
        ];
        runtime().block_on(async move {
            let cx = Cx::current().expect("ambient Cx");
            let http = Client::default_for_runtime(&cx);
            let mut session = Session::open(config(), cache, http)
                .unwrap()
                .with_endpoints(endpoints);
            let mut store = MemStore::default();
            let mut misses = AlbumMisses::default();
            let config = DjConfig::default();
            let started = Instant::now();
            let (works, first) = complete_works(
                &mut session,
                &cx,
                &mut store,
                works_of(&items),
                &config,
                NOW,
                &mut misses,
            )
            .await;
            let first_took = started.elapsed();
            let asked = || requests(&state, "/v1/albums/");
            let after_first = asked();
            let cached = store.album_tracks(ALBUM).unwrap().unwrap();
            let (_, second) = complete_works(
                &mut session,
                &cx,
                &mut store,
                works_of(&items),
                &config,
                NOW + 3600,
                &mut misses,
            )
            .await;
            let after_second = asked();
            let (_, week_later) = complete_works(
                &mut session,
                &cx,
                &mut store,
                works_of(&items),
                &config,
                NOW + MISS_RETRY_SECS,
                &mut misses,
            )
            .await;
            let after_week = asked();
            let offline = complete_from_cache(&store, works_of(&items)).unwrap();
            let disabled = DjConfig {
                expand_partial_works: false,
                ..DjConfig::default()
            };
            let (untouched, off) = complete_works(
                &mut session,
                &cx,
                &mut store,
                works_of(&items),
                &disabled,
                NOW,
                &mut AlbumMisses::default(),
            )
            .await;
            Outcome {
                works,
                first,
                first_took,
                cached,
                second,
                week_later,
                offline,
                untouched,
                off,
                asked: [after_first, after_second, after_week, asked()],
            }
        })
    }

    #[test]
    fn albums_are_read_once_through_429_and_paging_then_cached() {
        let (spotify, cache, data_dir) = authorized("expand-fetch");
        let run = fetch_scenario(&spotify, cache);

        // A 429 (waited out), the Mahler album's two pages, the missing 404.
        assert_eq!(run.asked[0], 4);
        assert!(
            run.first_took >= Duration::from_secs(1),
            "waited out Retry-After: {:?}",
            run.first_took
        );
        assert_eq!((run.first.completed, run.first.fetched), (1, 1));
        assert_eq!(run.first.failed.len(), 1, "the missing album is reported");
        let (album, why) = &run.first.failed[0];
        assert!(album == MISSING && why.contains("404"), "{why}");
        assert_eq!(run.first.halted, None, "a 404 is that album's alone");
        assert_eq!((run.cached.tracks.len(), run.cached.fetched_at), (6, NOW));
        let mahler = run
            .works
            .iter()
            .find(|w| w.album_uri.as_deref() == Some(ALBUM))
            .unwrap();
        assert_eq!(
            (mahler.movements.len(), mahler.completeness),
            (5, Completeness::Complete)
        );
        let elsewhere = run
            .works
            .iter()
            .find(|w| w.album_uri.as_deref() == Some(MISSING))
            .unwrap();
        assert_eq!(
            elsewhere.movements.len(),
            1,
            "an unreadable album's work plays as held"
        );

        // A cache hit makes no requests, and the missing album is left alone
        // for a week, then asked again.
        assert_eq!((run.second.completed, run.second.fetched), (1, 0));
        assert_eq!((run.second.skipped, run.second.failed.len()), (1, 0));
        assert_eq!(run.asked[1], 4);
        assert_eq!(run.asked[2], 5);
        assert_eq!(
            (run.week_later.skipped, run.week_later.failed.len()),
            (0, 1)
        );
        assert!(
            run.offline.iter().any(|w| w.movements.len() == 5),
            "the cache serves startup"
        );
        assert!(run.untouched.iter().all(|w| w.movements.len() == 1));
        assert_eq!(
            (run.off, run.asked[3]),
            (Expansion::default(), 5),
            "off reads nothing"
        );

        spotify.stop();
        std::fs::remove_dir_all(&data_dir).unwrap();
    }

    #[test]
    fn a_rate_limit_stops_the_run_and_unplayable_albums_wait() {
        let (spotify, cache, data_dir) = authorized("expand-halt");
        // One more 429 than a request waits out.
        spotify.state.lock().unwrap().rate_limit_albums = 4;
        let endpoints = spotify.endpoints();
        let state = Arc::clone(&spotify.state);
        let items = [
            liked(ADAGIETTO, ALBUM),
            liked("spotify:track:FakeElsewhere0000000001", MISSING),
            liked("spotify:track:FakeUnplayable00000001", UNPLAYABLE),
        ];
        let (stopped, took, asked, next, misses) = runtime().block_on(async move {
            let cx = Cx::current().expect("ambient Cx");
            let http = Client::default_for_runtime(&cx);
            let mut session = Session::open(config(), cache, http)
                .unwrap()
                .with_endpoints(endpoints);
            let mut store = MemStore::default();
            let mut misses = AlbumMisses::default();
            let config = DjConfig::default();
            let started = Instant::now();
            let (works, stopped) = complete_works(
                &mut session,
                &cx,
                &mut store,
                works_of(&items),
                &config,
                NOW,
                &mut misses,
            )
            .await;
            let took = started.elapsed();
            assert!(works.iter().all(|w| w.movements.len() == 1), "all held");
            assert!(misses.is_empty(), "a rate limit is no album's fault");
            let asked = requests(&state, "/v1/albums/");
            let (_, next) = complete_works(
                &mut session,
                &cx,
                &mut store,
                works_of(&items),
                &config,
                NOW + 60,
                &mut misses,
            )
            .await;
            (stopped, took, asked, next, misses)
        });

        // Whichever album came first used up its waits; no other was asked.
        assert_eq!(asked, 4);
        assert!(took >= Duration::from_secs(3), "three waits: {took:?}");
        assert!(
            stopped
                .halted
                .as_deref()
                .is_some_and(|why| why.contains("429")),
            "{stopped:?}"
        );
        assert_eq!(
            (stopped.completed, stopped.fetched, stopped.failed.len()),
            (0, 0, 0)
        );

        // Next run everything is tried: the Adagietto completes, the missing
        // and the unplayable albums are reported and remembered.
        assert_eq!((next.completed, next.fetched, next.halted), (1, 1, None));
        let mut failed = next.failed.clone();
        failed.sort();
        assert_eq!(failed.len(), 2, "{failed:?}");
        assert!(
            failed[0].0 == MISSING && failed[0].1.contains("404"),
            "{failed:?}"
        );
        assert_eq!(
            failed[1],
            (UNPLAYABLE.to_owned(), "no playable tracks".to_owned())
        );
        assert_eq!(misses.len(), 2, "both wait a week");

        // What stops a run, and what is one album's problem.
        assert!(halts(&SpotifyError::Http("connection refused".into())));
        assert!(halts(&SpotifyError::Api {
            status: 503,
            body: String::new()
        }));
        assert!(!halts(&SpotifyError::Api {
            status: 404,
            body: String::new()
        }));
        assert!(!halts(&SpotifyError::Decode("paging loop".into())));

        spotify.stop();
        std::fs::remove_dir_all(&data_dir).unwrap();
    }
}
