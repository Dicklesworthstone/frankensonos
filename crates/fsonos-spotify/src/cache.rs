//! The library cache: the owner's Spotify library persisted in the store's
//! `spotify_library` table, so the DJ starts from disk instead of a full
//! re-read. [`sync_library`] reads the library and writes it;
//! [`pool_from_store`] rebuilds the DJ's pool from the cached rows.
//!
//! Read-only on the Spotify side: syncing never starts playback or changes
//! the owner's library.

use std::collections::HashSet;

use asupersync::Cx;
use fsonos_core::store::{LibraryEntry, LibraryOrigin, Store};

use crate::SpotifyError;
use crate::classical::{CandidatePool, ClassicalTrack};
use crate::library::{ARTIST_SEPARATOR, LibraryItem, Origin, merge_duplicates, split_artists};
use crate::session::Session;

/// What one sync did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LibrarySync {
    /// Distinct tracks read from Spotify.
    pub tracks: usize,
    /// Of those, DJ candidates (classical, not explicit).
    pub classical: usize,
    /// Cached candidates no longer in the library, now retired.
    pub retired: usize,
}

/// Read the owner's whole library from Spotify and write it to the store.
/// Nothing is written unless the read completes.
pub async fn sync_library<S: Store + ?Sized>(
    session: &mut Session,
    cx: &Cx,
    store: &mut S,
) -> Result<LibrarySync, SpotifyError> {
    let items = session.read_library(cx).await?;
    apply_library_read(store, &items)
}

/// Write a *complete* library read to the store. Tracks the owner has since
/// un-saved stay cached but stop being DJ candidates (`is_classical = false`),
/// so the DJ never plays them; a partial read would wrongly retire whatever
/// it missed.
pub fn apply_library_read<S: Store + ?Sized>(
    store: &mut S,
    items: &[LibraryItem],
) -> Result<LibrarySync, SpotifyError> {
    let items = merge_duplicates(items);
    let pool = CandidatePool::build(&items);
    let mut entries: Vec<LibraryEntry> = items
        .iter()
        .map(|item| to_entry(item, pool.get(&item.source_uri)))
        .collect();
    let read: HashSet<&str> = items.iter().map(|i| i.source_uri.as_str()).collect();
    let mut retired = 0;
    for mut cached in store.library()? {
        if cached.is_classical && !read.contains(cached.track.source_uri.as_str()) {
            cached.is_classical = false;
            entries.push(cached);
            retired += 1;
        }
    }
    store.upsert_library(&entries)?;
    Ok(LibrarySync {
        tracks: items.len(),
        classical: pool.len(),
        retired,
    })
}

/// The DJ's pool from the cache's candidate rows, trusting their
/// `is_classical` verdict (the cache keeps no genres or label to re-judge
/// with).
pub fn pool_from_store<S: Store + ?Sized>(store: &S) -> Result<CandidatePool, SpotifyError> {
    let items: Vec<LibraryItem> = store
        .library()?
        .iter()
        .filter(|entry| entry.is_classical)
        .map(from_entry)
        .collect();
    Ok(CandidatePool::from_classical(&items))
}

/// A library item as a cache row. `candidate` is the item's analysis when
/// the DJ may play it; it marks the row classical and stamps its work key.
#[must_use]
pub fn to_entry(item: &LibraryItem, candidate: Option<&ClassicalTrack>) -> LibraryEntry {
    let candidate = candidate.filter(|_| !item.explicit);
    LibraryEntry {
        track: item.to_track(),
        is_classical: candidate.is_some(),
        added: item.added_at.unwrap_or(0),
        album_uri: item.album_uri.clone(),
        album_artists: (!item.album_artists.is_empty())
            .then(|| item.album_artists.join(ARTIST_SEPARATOR)),
        origin: match item.origin {
            Origin::SavedAlbum => LibraryOrigin::SavedAlbum,
            Origin::LikedTrack => LibraryOrigin::LikedTrack,
            Origin::Both => LibraryOrigin::Both,
        },
        disc_number: item.disc_number,
        track_number: item.track_number,
        work_key: candidate.map(|t| t.work_key.clone()),
    }
}

/// A cache row as a library item. Genres, label and the explicit flag are
/// not cached; explicit rows are never candidates anyway.
#[must_use]
pub fn from_entry(entry: &LibraryEntry) -> LibraryItem {
    let origin = match entry.origin {
        LibraryOrigin::SavedAlbum => Origin::SavedAlbum,
        LibraryOrigin::LikedTrack => Origin::LikedTrack,
        LibraryOrigin::Both => Origin::Both,
    };
    let mut item = LibraryItem::from_track(&entry.track, origin);
    item.album_uri.clone_from(&entry.album_uri);
    item.album_artists = entry
        .album_artists
        .as_deref()
        .map(split_artists)
        .unwrap_or_default();
    item.disc_number = entry.disc_number;
    item.track_number = entry.track_number;
    item.added_at = (entry.added != 0).then_some(entry.added);
    item
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use asupersync::http::Client;
    use fsonos_core::store::{MemStore, SqliteStore};

    use super::*;
    use crate::classical::analyze;
    use crate::client::{CachedToken, Paging, SCOPE, SavedAlbum, SavedTrack, TokenCache};
    use crate::fake_spotify::{FakeSpotify, config, runtime, scratch_dir};

    fn fixture_items() -> Vec<LibraryItem> {
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
        // A non-classical liked track and an explicit one.
        let mut pop = items.last().unwrap().clone();
        pop.source_uri = "spotify:track:FakePop000000000000001".into();
        pop.title = "Shape of You".into();
        pop.artists = vec!["Ed Sheeran".into()];
        pop.album = Some("÷".into());
        pop.album_uri = Some("spotify:album:FakePopAlbum0000000001".into());
        pop.album_artists = vec!["Ed Sheeran".into()];
        items.push(pop);
        let mut explicit = items[0].clone();
        explicit.source_uri = "spotify:track:FakeExplicit0000000001".into();
        explicit.explicit = true;
        items.push(explicit);
        items
    }

    /// Pool members, sorted: the cache returns rows by (added, uri), so a
    /// rebuilt pool holds the same tracks in a different (still stable) order.
    fn uris(pool: &CandidatePool) -> Vec<&str> {
        let mut uris: Vec<&str> = pool
            .tracks()
            .iter()
            .map(|t| t.track.source_uri.as_str())
            .collect();
        uris.sort_unstable();
        uris
    }

    #[test]
    fn entries_round_trip() {
        let mut item = fixture_items()[0].clone();
        item.origin = Origin::Both;
        let analysis = analyze(&item);
        let entry = to_entry(&item, Some(&analysis));
        assert_eq!(entry.work_key.as_deref(), Some(analysis.work_key.as_str()));
        assert!(!to_entry(&item, None).is_classical);
        assert_eq!(to_entry(&item, None).work_key, None);
        let back = from_entry(&entry);
        assert_eq!(back.source_uri, item.source_uri);
        assert_eq!(back.title, item.title);
        assert_eq!(back.artists, item.artists);
        assert_eq!(back.album, item.album);
        assert_eq!(back.album_uri, item.album_uri);
        assert_eq!(back.album_artists, item.album_artists);
        assert_eq!((back.disc_number, back.track_number), (Some(1), Some(1)));
        assert_eq!(back.added_at, item.added_at);
        assert_eq!(back.duration_secs, item.duration_secs);
        assert_eq!(back.origin, Origin::Both);
        let mut explicit = item.clone();
        explicit.explicit = true;
        let explicit_entry = to_entry(&explicit, Some(&analysis));
        assert!(!explicit_entry.is_classical && explicit_entry.work_key.is_none());
    }

    #[test]
    fn sync_writes_candidates_and_retires_unsaved_tracks_in_fsqlite() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let items = fixture_items();
        let report = apply_library_read(&mut store, &items).unwrap();
        // 3 Goldberg + 1 Chopin + 1 Debussy + pop + explicit.
        assert_eq!(
            report,
            LibrarySync {
                tracks: 7,
                classical: 5,
                retired: 0
            }
        );

        let rows = store.library().unwrap();
        assert_eq!(rows.len(), 7);
        let row = |uri: &str| rows.iter().find(|r| r.track.source_uri == uri).unwrap();
        assert!(!row("spotify:track:FakePop000000000000001").is_classical);
        assert!(!row("spotify:track:FakeExplicit0000000001").is_classical);
        let clair = row("spotify:track:FakeTrack0000000000006");
        assert!(clair.is_classical);
        assert_eq!(clair.origin, LibraryOrigin::LikedTrack);
        assert_eq!(clair.track_number, Some(3));
        assert_eq!(
            clair.work_key.as_deref(),
            Some("claude debussy|suite bergamasque l 75")
        );
        assert_eq!(row("spotify:track:FakePop000000000000001").work_key, None);

        // The cache rebuilds the same pool the read produced.
        let rebuilt = pool_from_store(&store).unwrap();
        assert_eq!(uris(&rebuilt), uris(&CandidatePool::build(&items)));
        let aria = rebuilt.get("spotify:track:FakeTrack0000000000001").unwrap();
        assert_eq!(aria.composer, "Johann Sebastian Bach");
        assert_eq!(
            aria.album_uri.as_deref(),
            Some("spotify:album:FakeAlbum0000000000001")
        );

        // The owner un-saves the Debussy: it stays cached but leaves the pool.
        let remaining: Vec<LibraryItem> = items
            .iter()
            .filter(|i| i.source_uri != "spotify:track:FakeTrack0000000000006")
            .cloned()
            .collect();
        let report = apply_library_read(&mut store, &remaining).unwrap();
        assert_eq!(
            report,
            LibrarySync {
                tracks: 6,
                classical: 4,
                retired: 1
            }
        );
        assert_eq!(store.library().unwrap().len(), 7);
        let pool = pool_from_store(&store).unwrap();
        assert_eq!(pool.len(), 4);
        assert!(pool.get("spotify:track:FakeTrack0000000000006").is_none());

        // Re-syncing the same read retires nothing more.
        assert_eq!(
            apply_library_read(&mut store, &remaining).unwrap().retired,
            0
        );
    }

    #[test]
    fn sync_library_end_to_end_against_fake_spotify() {
        let spotify = FakeSpotify::start();
        let data_dir = scratch_dir("cache-sync");
        let cache = TokenCache::in_data_dir(&data_dir);
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
        }
        let endpoints = spotify.endpoints();
        let state = Arc::clone(&spotify.state);

        let (first, second, pool_len) = runtime().block_on(async move {
            let cx = Cx::current().expect("ambient Cx");
            let http = Client::default_for_runtime(&cx);
            let mut session = Session::open(config(), cache, http)
                .unwrap()
                .with_endpoints(endpoints);
            let mut store = MemStore::default();
            let first = sync_library(&mut session, &cx, &mut store).await.unwrap();
            // The owner un-likes everything; the next sync retires it.
            state.lock().unwrap().liked_tracks =
                Some(r#"{"items":[],"next":null,"offset":0,"limit":50,"total":0}"#.into());
            let second = sync_library(&mut session, &cx, &mut store).await.unwrap();
            (first, second, pool_from_store(&store).unwrap().len())
        });
        spotify.stop();

        // 3 Goldberg + 1 Chopin (the other is unplayable) + the rest of the
        // long Chopin album + the liked Debussy.
        assert_eq!(
            first,
            LibrarySync {
                tracks: 6,
                classical: 6,
                retired: 0
            }
        );
        assert_eq!(
            second,
            LibrarySync {
                tracks: 5,
                classical: 5,
                retired: 1
            }
        );
        assert_eq!(pool_len, 5);
        std::fs::remove_dir_all(&data_dir).unwrap();
    }
}
