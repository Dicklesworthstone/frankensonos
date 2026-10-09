//! Genre tags for the owner's library, from the artists.
//!
//! Spotify leaves album `genres` empty for almost every album; artists carry
//! the tags. So a sync reads each lead artist's genres (`GET /artists/{id}`,
//! read-only, the token the library read already uses) and unions them into
//! the artist's tracks, so songs carry genres to steer, balance and prefer
//! by. Development-mode apps lost the batch endpoint in February 2026, so it
//! is one request per artist; the answers are cached in the store and
//! re-read after [`REFRESH_SECS`], a sync reads at most
//! [`MAX_READS_PER_SYNC`] artists, and a rate limit past the waits (or a
//! server error, or being signed out) stops the run: the rest wait for the
//! next sync, as album expansion does.

use std::collections::{HashMap, HashSet};

use asupersync::Cx;
use fsonos_core::store::Store;

use crate::SpotifyError;
use crate::expand::halts;
use crate::library::LibraryItem;
use crate::session::Session;

/// How long an artist's cached genres are trusted (90 days).
pub const REFRESH_SECS: i64 = 90 * 86_400;
/// The most artists one sync reads; the rest wait for the next.
pub const MAX_READS_PER_SYNC: usize = 250;

/// What one genre read did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GenreRead {
    /// Artists read from the Web API (an unknown id counts: it is cached as
    /// having no genres).
    pub fetched: usize,
    /// Artists answered from the cache.
    pub cached: usize,
    /// Artists left for the next sync (the per-sync cap, or the run halted).
    pub deferred: usize,
    /// Artists whose genres couldn't be read or cached, and why.
    pub failed: Vec<(String, String)>,
    /// Why the run stopped early, if it did.
    pub halted: Option<String>,
}

/// Give `items` their lead artists' genre tags, reading the artists the
/// store hasn't cached (or cached more than [`REFRESH_SECS`] ago). Album
/// genres the read gave are kept; tags are not repeated. An artist that
/// can't be read now keeps its stale tags, if any.
pub async fn tag_genres<S: Store + ?Sized>(
    session: &mut Session,
    cx: &Cx,
    store: &mut S,
    items: &mut [LibraryItem],
    now: i64,
) -> GenreRead {
    let mut report = GenreRead::default();
    let mut seen = HashSet::new();
    let ids: Vec<String> = items
        .iter()
        .filter_map(|i| i.artist_id.clone())
        .filter(|id| seen.insert(id.clone()))
        .collect();
    let mut genres: HashMap<String, Vec<String>> = HashMap::new();
    let mut reads = 0;
    for id in ids {
        let stale = match store.artist_genres(&id) {
            Ok(Some((tags, at))) if now.saturating_sub(at) < REFRESH_SECS => {
                report.cached += 1;
                genres.insert(id, tags);
                continue;
            }
            Ok(cached) => cached.map(|(tags, _)| tags),
            Err(e) => {
                report.failed.push((id, SpotifyError::from(e).to_string()));
                continue;
            }
        };
        if report.halted.is_some() || reads >= MAX_READS_PER_SYNC {
            report.deferred += 1;
            genres.extend(stale.map(|tags| (id, tags)));
            continue;
        }
        reads += 1;
        let tags = match session.read_artist_genres(cx, &id).await {
            Ok(tags) => tags,
            // An id the Web API doesn't know: remember it has no genres.
            Err(SpotifyError::Api { status: 404, .. }) => Vec::new(),
            Err(e) => {
                if halts(&e) {
                    report.halted = Some(e.to_string());
                    report.deferred += 1;
                } else {
                    report.failed.push((id.clone(), e.to_string()));
                }
                genres.extend(stale.map(|tags| (id, tags)));
                continue;
            }
        };
        report.fetched += 1;
        if let Err(e) = store.save_artist_genres(&id, &tags, now) {
            report
                .failed
                .push((id.clone(), SpotifyError::from(e).to_string()));
        }
        genres.insert(id, tags);
    }
    for item in items.iter_mut() {
        let Some(tags) = item.artist_id.as_ref().and_then(|id| genres.get(id)) else {
            continue;
        };
        for tag in tags {
            if !item.genres.contains(tag) {
                item.genres.push(tag.clone());
            }
        }
    }
    report
}

#[cfg(test)]
mod tests {
    //! Against the fake Spotify on loopback (`crate::fake_spotify`).

    use asupersync::http::Client;
    use fsonos_core::store::MemStore;

    use super::*;
    use crate::client::{CachedToken, Paging, SCOPE, SavedAlbum, SavedTrack, TokenCache};
    use crate::fake_spotify::{Fake, FakeSpotify, config, runtime, scratch_dir};

    const NOW: i64 = 1_790_035_200;

    /// The fixture library, as a read returns it (album genres empty).
    fn library() -> Vec<LibraryItem> {
        let albums =
            Paging::<SavedAlbum>::parse(include_bytes!("../tests/fixtures/saved_albums_page.json"))
                .unwrap();
        let liked =
            Paging::<SavedTrack>::parse(include_bytes!("../tests/fixtures/saved_tracks_page.json"))
                .unwrap();
        let mut items: Vec<LibraryItem> = albums
            .items
            .iter()
            .flat_map(SavedAlbum::library_items)
            .collect();
        items.extend(liked.items.iter().filter_map(SavedTrack::library_item));
        items
    }

    /// A fake Spotify signed in as the owner, with genres for Bach and
    /// Chopin (Debussy is unknown to it: a 404).
    fn spotify(name: &str) -> (FakeSpotify, TokenCache, std::path::PathBuf) {
        let spotify = FakeSpotify::start();
        let dir = scratch_dir(name);
        let cache = TokenCache::in_data_dir(&dir);
        cache
            .store(&CachedToken {
                access_token: "access-1".into(),
                refresh_token: "refresh-1".into(),
                expires_at: i64::MAX / 2,
                scope: SCOPE.into(),
            })
            .unwrap();
        {
            let mut fake = spotify.state.lock().unwrap();
            "access-1".clone_into(&mut fake.access);
            "refresh-1".clone_into(&mut fake.refresh);
            for (id, tags) in [
                ("FakeArtist000000000001", vec!["baroque", "early music"]),
                ("FakeArtist000000000003", vec!["romantic era"]),
                ("FakeArtist000000000006", vec!["late romantic era"]),
            ] {
                fake.artist_genres
                    .insert(id.into(), tags.into_iter().map(str::to_owned).collect());
            }
        }
        (spotify, cache, dir)
    }

    fn artist_gets(fake: &Fake) -> usize {
        fake.log
            .iter()
            .filter(|l| l.contains("/v1/artists/"))
            .count()
    }

    #[test]
    fn artists_are_read_once_and_their_genres_tag_the_library() {
        let (spotify, cache, dir) = spotify("genres-once");
        let endpoints = spotify.endpoints();
        let lead_ids: HashSet<String> = library()
            .iter()
            .filter_map(|i| i.artist_id.clone())
            .collect();
        assert!(lead_ids.len() >= 3, "{lead_ids:?}");

        let (first, items, second, refreshed, store) = runtime().block_on(async move {
            let cx = Cx::current().expect("ambient Cx");
            let http = Client::default_for_runtime(&cx);
            let mut session = Session::open(config(), cache, http)
                .unwrap()
                .with_endpoints(endpoints);
            let mut store = MemStore::default();
            let mut items = library();
            let first = tag_genres(&mut session, &cx, &mut store, &mut items, NOW).await;
            let second = tag_genres(&mut session, &cx, &mut store, &mut library(), NOW + 60).await;
            let refreshed = tag_genres(
                &mut session,
                &cx,
                &mut store,
                &mut library(),
                NOW + REFRESH_SECS,
            )
            .await;
            (first, items, second, refreshed, store)
        });
        let fake = spotify.stop();

        // Each lead artist once; Debussy's 404 is cached as "no genres"
        // (and re-read with the rest at the refresh).
        assert_eq!(first.fetched, lead_ids.len(), "{first:?}");
        assert!(
            first.failed.is_empty() && first.halted.is_none(),
            "{first:?}"
        );
        let aria = items
            .iter()
            .find(|i| i.source_uri == "spotify:track:FakeTrack0000000000001")
            .unwrap();
        assert_eq!(aria.genres, ["baroque", "early music"]);
        let clair = items
            .iter()
            .find(|i| i.source_uri == "spotify:track:FakeTrack0000000000006")
            .unwrap();
        assert!(clair.genres.is_empty(), "the Web API had none for Debussy");
        assert_eq!(
            store.artist_genres("FakeArtist000000000004").unwrap(),
            Some((Vec::new(), NOW + REFRESH_SECS))
        );
        // The next sync reads nothing; one past the refresh period reads all.
        assert_eq!((second.fetched, second.cached), (0, lead_ids.len()));
        assert_eq!(refreshed.fetched, lead_ids.len());
        assert_eq!(artist_gets(&fake), 2 * lead_ids.len());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_rate_limit_halts_the_read_and_the_next_sync_resumes() {
        let (spotify, cache, dir) = spotify("genres-429");
        // More 429s than one GET waits out.
        spotify.state.lock().unwrap().rate_limit_artists = 4;
        let endpoints = spotify.endpoints();
        let lead_ids = library()
            .iter()
            .filter_map(|i| i.artist_id.clone())
            .collect::<HashSet<_>>()
            .len();

        let (halted, untagged, resumed, tagged) = runtime().block_on(async move {
            let cx = Cx::current().expect("ambient Cx");
            let http = Client::default_for_runtime(&cx);
            let mut session = Session::open(config(), cache, http)
                .unwrap()
                .with_endpoints(endpoints);
            let mut store = MemStore::default();
            let mut items = library();
            let halted = tag_genres(&mut session, &cx, &mut store, &mut items, NOW).await;
            let untagged = items.iter().all(|i| i.genres.is_empty());
            let mut again = library();
            let resumed = tag_genres(&mut session, &cx, &mut store, &mut again, NOW + 60).await;
            let tagged = again.iter().any(|i| !i.genres.is_empty());
            (halted, untagged, resumed, tagged)
        });
        spotify.stop();

        assert!(halted.halted.is_some(), "{halted:?}");
        assert_eq!((halted.fetched, halted.deferred), (0, lead_ids));
        assert!(untagged, "nothing read, nothing tagged");
        assert_eq!(resumed.fetched, lead_ids, "{resumed:?}");
        assert!(resumed.halted.is_none() && tagged);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
