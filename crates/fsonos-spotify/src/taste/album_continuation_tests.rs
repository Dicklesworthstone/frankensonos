//! Whole-work recovery through real HTTP, durable checkpoints and SQLite.

use super::*;
use crate::cache::{
    ALBUM_PAGE_BUDGET, AlbumContinuation, LibrarySyncDetailed, continue_albums,
    sync_library_detailed, works_from_store,
};
use fsonos_core::store::SqliteStore;

const OWNER: &str = "test-owner";
const ALBUM: &str = "BackgroundSonata";
const ROUTE: &str = "/v1/albums/BackgroundSonata/tracks";

fn movement(album: &str, number: u32) -> Value {
    let numeral =
        ["I", "II", "III", "IV", "V", "VI", "VII", "VIII", "IX", "X"][number as usize - 1];
    json!({
        "uri": format!("spotify:track:{album}Movement{number}"),
        "name": format!("Piano Sonata No. 14 in C-Sharp Minor, Op. 27 No. 2: {numeral}. Allegro"),
        "artists": [{ "name": "Ludwig van Beethoven" }, { "name": "Test pianist" }],
        "album": {
            "name": "Test sonata recording", "uri": format!("spotify:album:{album}"),
            "artists": [{ "name": "Ludwig van Beethoven" }]
        },
        "duration_ms": 180_000, "is_playable": true,
        "disc_number": 1, "track_number": number
    })
}

fn held(album: &str) -> LibraryItem {
    LibraryItem {
        source_uri: format!("spotify:track:{album}Movement2"),
        title: "Piano Sonata No. 14 in C-Sharp Minor, Op. 27 No. 2: II. Allegro".into(),
        artists: vec!["Ludwig van Beethoven".into(), "Test pianist".into()],
        album: Some("Test sonata recording".into()),
        album_uri: Some(format!("spotify:album:{album}")),
        album_artists: vec!["Ludwig van Beethoven".into()],
        disc_number: Some(1),
        track_number: Some(2),
        duration_secs: Some(180),
        origin: Origin::LikedTrack,
        ..LibraryItem::default()
    }
}

fn album_pages(server: &Server, album: &str, count: u32) {
    for offset in 0..count {
        server.set(
            &format!("/v1/albums/{album}/tracks?offset={offset}"),
            json!({
                "items": [movement(album, offset + 1)],
                "offset": offset, "total": count,
                "next": (offset + 1 < count).then(|| {
                    format!("{API}/albums/{album}/tracks?offset={}", offset + 1)
                })
            }),
        );
    }
}

async fn continue_with(
    server: &Server,
    cx: &Cx,
    store: &mut dyn Store,
    previous: Option<&AlbumContinuation>,
) -> LibrarySyncDetailed {
    let mut session = server.session(cx, "user-library-read");
    continue_albums(&mut session, cx, store, previous)
        .await
        .unwrap()
}

#[test]
fn full_sync_discovers_a_rate_limited_work_and_album_only_recovery_completes_it() {
    let server = Server::new();
    server.set(
        "/v1/me/tracks",
        page(vec![json!({ "track": movement(ALBUM, 2) })], 0, None),
    );
    album_pages(&server, ALBUM, 3);
    server
        .state
        .lock()
        .unwrap()
        .refused
        .insert(ROUTE.into(), 429);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        let mut session = server.session(&cx, "user-library-read");
        let full = sync_library_detailed(&mut session, &cx, &mut store)
            .await
            .unwrap();
        assert_eq!(full.albums.pending, 1);
        assert!(full.playlists.retry_after_secs > 0);
        assert!(
            store
                .album_tracks(&format!("spotify:album:{ALBUM}"))
                .unwrap()
                .is_none()
        );
        let liked = store.library().unwrap();
        let before = server.calls("/v1/").len();
        server.state.lock().unwrap().refused.remove(ROUTE);
        let recovered = continue_with(&server, &cx, &mut store, Some(&full.albums)).await;
        assert_eq!(
            (recovered.albums.completed, recovered.albums.pending),
            (1, 0)
        );
        let pool = works_from_store(&store).unwrap();
        let work = pool.work_of(&held(ALBUM).source_uri).unwrap();
        assert_eq!(work.movements.len(), 3);
        assert_eq!(work.completeness, crate::works::Completeness::Complete);
        assert_eq!(
            store.library().unwrap(),
            liked,
            "catalog completion changes no saves"
        );
        assert!(
            server.calls("/v1/")[before..]
                .iter()
                .all(|call| matches!(path(call), "/v1/me" | ROUTE)),
            "no library, playlist or genre reread is needed"
        );
    });
}

#[test]
fn a_long_album_resumes_its_validated_pages_after_restart_before_any_cache_publication() {
    let server = Server::new();
    let count = u32::try_from(ALBUM_PAGE_BUDGET + 2).unwrap();
    album_pages(&server, ALBUM, count);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let file = server.data.join("background-albums.db");
        let mut store = SqliteStore::open(&file).unwrap();
        store.adopt_spotify_library_owner(OWNER).unwrap();
        apply_library_read(&mut store, &[held(ALBUM)]).unwrap();
        let first = continue_with(&server, &cx, &mut store, None).await;
        assert_eq!(server.calls(ROUTE).len(), ALBUM_PAGE_BUDGET);
        assert_eq!((first.albums.pending, first.albums.completed), (1, 0));
        assert_eq!(first.albums.advanced, ALBUM_PAGE_BUDGET);
        assert!(
            store
                .album_tracks(&format!("spotify:album:{ALBUM}"))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            works_from_store(&store)
                .unwrap()
                .work_of(&held(ALBUM).source_uri)
                .unwrap()
                .movements
                .len(),
            1,
            "a partial list cannot masquerade as a complete recording"
        );
        let saved = serde_json::to_string(&first.albums).unwrap();
        store.close().unwrap();

        let previous: AlbumContinuation = serde_json::from_str(&saved).unwrap();
        let mut store = SqliteStore::open(&file).unwrap();
        let result = continue_with(&server, &cx, &mut store, Some(&previous)).await;
        assert_eq!((result.albums.completed, result.albums.pending), (1, 0));
        let calls = server.calls(ROUTE);
        assert_eq!(
            calls.len(),
            count as usize,
            "no earlier pages were fetched again"
        );
        assert!(calls[ALBUM_PAGE_BUDGET].contains("offset=8"));
        let album = store
            .album_tracks(&format!("spotify:album:{ALBUM}"))
            .unwrap()
            .unwrap();
        assert_eq!(album.tracks.len(), count as usize);
        assert_eq!(
            works_from_store(&store)
                .unwrap()
                .work_of(&held(ALBUM).source_uri)
                .unwrap()
                .movements
                .len(),
            count as usize
        );
        let before = server.calls("/v1/").len();
        let done = continue_with(&server, &cx, &mut store, Some(&result.albums)).await;
        assert_eq!(done.albums.pending, 0);
        assert_eq!(
            server.calls("/v1/").len(),
            before,
            "completed work stays quiet"
        );
        store.close().unwrap();
    });
}

#[test]
fn a_malformed_album_does_not_starve_later_recordings_or_poison_its_cache() {
    let server = Server::new();
    let bad = "ABrokenSonata";
    let good = "ZCompleteSonata";
    server.set(
        &format!("/v1/albums/{bad}/tracks"),
        json!({ "items": [], "offset": 0, "total": 3, "next": null }),
    );
    album_pages(&server, good, 3);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        store.adopt_spotify_library_owner(OWNER).unwrap();
        apply_library_read(&mut store, &[held(bad), held(good)]).unwrap();
        let result = continue_with(&server, &cx, &mut store, None).await;
        assert_eq!((result.albums.completed, result.albums.pending), (1, 1));
        assert!(
            store
                .album_tracks(&format!("spotify:album:{bad}"))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store
                .album_tracks(&format!("spotify:album:{good}"))
                .unwrap()
                .unwrap()
                .tracks
                .len(),
            3
        );
        assert!(
            store.album_misses().unwrap().is_empty(),
            "bad paging is not a week-long miss"
        );
        album_pages(&server, bad, 3);
        let repaired = continue_with(&server, &cx, &mut store, Some(&result.albums)).await;
        assert_eq!((repaired.albums.completed, repaired.albums.pending), (1, 0));
    });
}

#[test]
fn retained_pages_are_discarded_when_the_advertised_total_changes() {
    let server = Server::new();
    album_pages(&server, ALBUM, 10);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        store.adopt_spotify_library_owner(OWNER).unwrap();
        apply_library_read(&mut store, &[held(ALBUM)]).unwrap();
        let first = continue_with(&server, &cx, &mut store, None).await;
        server.set(
            &format!("{ROUTE}?offset=8"),
            json!({ "items": [movement(ALBUM, 9)], "offset": 8, "total": 9, "next": null }),
        );
        let changed = continue_with(&server, &cx, &mut store, Some(&first.albums)).await;
        assert_eq!((changed.albums.completed, changed.albums.advanced), (0, 0));
        assert!(
            store
                .album_tracks(&format!("spotify:album:{ALBUM}"))
                .unwrap()
                .is_none()
        );
        album_pages(&server, ALBUM, 3);
        let before = server.calls(ROUTE).len();
        let repaired = continue_with(&server, &cx, &mut store, Some(&changed.albums)).await;
        assert_eq!(repaired.albums.completed, 1);
        assert!(
            server.calls(ROUTE)[before].contains("offset=0"),
            "restart cleanly"
        );
    });
}

#[test]
fn a_later_account_adoption_cannot_consume_old_partial_album_work() {
    for during_request in [false, true] {
        let server = Server::new();
        album_pages(&server, ALBUM, 10);
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let file = server.data.join("album-account.db");
            let mut store = SqliteStore::open(&file).unwrap();
            let (original, _) = store.adopt_spotify_library_owner(OWNER).unwrap();
            apply_library_read(&mut store, &[held(ALBUM)]).unwrap();
            let first = continue_with(&server, &cx, &mut store, None).await;
            let switch = move || {
                let mut other = SqliteStore::open(&file).unwrap();
                other
                    .adopt_spotify_library_owner("another-test-owner")
                    .unwrap();
                let (returned, _) = other.adopt_spotify_library_owner(OWNER).unwrap();
                assert_ne!(returned.generation, original.generation);
                apply_library_read(&mut other, &[held(ALBUM)]).unwrap();
                other.close().unwrap();
            };
            if during_request {
                server.state.lock().unwrap().before_response =
                    Some((ROUTE.into(), Box::new(switch)));
            } else {
                switch();
            }
            let before = server.calls(ROUTE).len();
            let changed = continue_with(&server, &cx, &mut store, Some(&first.albums)).await;
            assert!(changed.albums.requires_refresh);
            assert_eq!(changed.albums.completed, 0);
            assert!(
                store
                    .album_tracks(&format!("spotify:album:{ALBUM}"))
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                server.calls(ROUTE).len() - before,
                usize::from(during_request)
            );
            let rebuilt = continue_with(&server, &cx, &mut store, None).await;
            assert!(!rebuilt.albums.requires_refresh);
            assert_eq!(
                rebuilt.albums.advanced, ALBUM_PAGE_BUDGET,
                "new adoption starts at zero"
            );
            store.close().unwrap();
        });
    }
}
