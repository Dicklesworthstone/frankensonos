//! Artist enrichment through real HTTP, durable checkpoints and reopened SQLite.

use super::*;
use crate::cache::{
    GENRE_READ_BUDGET, GenreContinuation, LibrarySyncDetailed, continue_genres, continue_playlists,
    sync_library_detailed, works_from_store,
};
use fsonos_core::store::SqliteStore;

const OWNER: &str = "test-owner";
const ARTISTS: &str = "/v1/artists/";

fn artist_id(number: usize) -> String {
    format!("GenreArtist{number:04}")
}

fn route(number: usize) -> String {
    format!("{ARTISTS}{}", artist_id(number))
}

fn held(number: usize) -> LibraryItem {
    LibraryItem {
        source_uri: format!("spotify:track:GenreTrack{number}"),
        title: format!("Test song {number}"),
        artists: vec![format!("Test performer {number}")],
        artist_id: Some(artist_id(number)),
        album: Some("Test album".into()),
        genres: vec!["folk".into()],
        added_at: Some(1_700_000_000),
        taste_pm: Some(1_250),
        duration_secs: Some(180),
        origin: Origin::LikedTrack,
        ..LibraryItem::default()
    }
}

fn api_track(number: usize) -> Value {
    let mut value = track(number);
    value["artists"] = json!([{
        "id": artist_id(number), "name": format!("Test performer {number}")
    }]);
    value
}

fn tags(server: &Server, number: usize, genres: &[&str]) {
    server.set(
        &route(number),
        json!({ "id": artist_id(number), "name": "Test performer", "genres": genres }),
    );
}

async fn continue_with(
    server: &Server,
    cx: &Cx,
    store: &mut dyn Store,
    previous: Option<&GenreContinuation>,
) -> LibrarySyncDetailed {
    let mut session = server.session(cx, "user-library-read");
    continue_genres(&mut session, cx, store, previous)
        .await
        .unwrap()
}

#[test]
fn a_large_library_resumes_after_restart_without_rereading_personal_data() {
    let server = Server::new();
    let count = GENRE_READ_BUDGET + 3;
    for number in 0..count {
        tags(&server, number, &["jazz", "folk", "jazz"]);
    }
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let file = server.data.join("background-genres.db");
        let checkpoint = server.data.join("genre-checkpoint.json");
        let mut store = SqliteStore::open(&file).unwrap();
        let (owner, _) = store.adopt_spotify_library_owner(OWNER).unwrap();
        apply_library_read(&mut store, &[held(999)]).unwrap();
        let mut items: Vec<_> = (0..count).map(held).collect();
        let mut another = held(0);
        another.source_uri = "spotify:track:AnotherGenreTrack".into();
        another.explicit = true;
        items.push(another);
        apply_library_read(&mut store, &items).unwrap();
        let accepted = store.library().unwrap();
        let first = continue_with(&server, &cx, &mut store, None).await;
        assert_eq!(server.calls(ARTISTS).len(), GENRE_READ_BUDGET);
        assert_eq!(first.genre_continuation.owner.as_deref(), Some(OWNER));
        assert_eq!(first.genre_continuation.generation, Some(owner.generation));
        assert_eq!(first.genre_continuation.completed, GENRE_READ_BUDGET);
        assert_eq!(first.genre_continuation.pending, 3);
        assert_eq!(first.genres.deferred, 3);
        assert_eq!(first.library.candidates, count);
        assert_eq!(store.library().unwrap(), accepted);
        assert!(
            server.calls(&route(999)).is_empty(),
            "retired artists create no work"
        );
        std::fs::write(
            &checkpoint,
            serde_json::to_vec(&first.genre_continuation).unwrap(),
        )
        .unwrap();
        store.close().unwrap();

        let previous: GenreContinuation =
            serde_json::from_slice(&std::fs::read(&checkpoint).unwrap()).unwrap();
        let mut store = SqliteStore::open(&file).unwrap();
        let done = continue_with(&server, &cx, &mut store, Some(&previous)).await;
        assert_eq!(
            (
                done.genre_continuation.completed,
                done.genre_continuation.pending
            ),
            (3, 0)
        );
        assert_eq!(done.genres.cached, GENRE_READ_BUDGET);
        assert_eq!(
            server.calls(ARTISTS).len(),
            count,
            "earlier artists are not fetched again"
        );
        assert_eq!(
            store.library().unwrap(),
            accepted,
            "enrichment changes no personal rows"
        );
        assert_eq!(store.spotify_library_owner().unwrap(), Some(owner));
        let pool = pool_from_store(&store).unwrap();
        let work_pool = works_from_store(&store).unwrap();
        for item in &items {
            let track = pool.get(&item.source_uri).unwrap();
            assert_eq!(track.genres, ["folk", "jazz"]);
            assert_eq!(track.explicit, item.explicit);
            let work = work_pool.work_of(&item.source_uri).unwrap();
            assert_eq!(work.genres(), ["folk", "jazz"]);
            assert_eq!(work.taste_pm(), 1_250);
        }
        let calls = server.calls("/v1/");
        assert!(
            calls
                .iter()
                .all(|call| path(call) == "/v1/me" || path(call).starts_with(ARTISTS))
        );
        let quiet = continue_with(&server, &cx, &mut store, Some(&done.genre_continuation)).await;
        assert_eq!(quiet.genre_continuation.pending, 0);
        assert_eq!(quiet.genre_continuation.completed, 0);
        assert_eq!(
            server.calls("/v1/"),
            calls,
            "completed metadata stays quiet"
        );
        store.close().unwrap();
    });
}

#[test]
fn repeated_failures_rotate_past_the_budget_and_retain_last_good_tags() {
    let server = Server::new();
    let count = GENRE_READ_BUDGET + 2;
    for number in 0..GENRE_READ_BUDGET {
        server.set(&route(number), json!({ "genres": "malformed" }));
    }
    for number in GENRE_READ_BUDGET..count {
        tags(&server, number, &["blues"]);
    }
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let file = server.data.join("failed-genres.db");
        let mut store = SqliteStore::open(&file).unwrap();
        store.adopt_spotify_library_owner(OWNER).unwrap();
        apply_library_read(&mut store, &(0..count).map(held).collect::<Vec<_>>()).unwrap();
        store
            .save_artist_genres(&artist_id(0), &["last good jazz".into()], 0)
            .unwrap();
        let saved = store.artist_genres(&artist_id(0)).unwrap();
        let accepted = store.library().unwrap();
        let first = continue_with(&server, &cx, &mut store, None).await;
        assert_eq!(first.genre_continuation.pending, count);
        assert_eq!(first.genre_continuation.completed, 0);
        assert_eq!(first.genres.failed.len(), GENRE_READ_BUDGET);
        assert_eq!(first.genres.deferred, 2);
        assert_eq!(store.artist_genres(&artist_id(0)).unwrap(), saved);
        assert_eq!(
            pool_from_store(&store)
                .unwrap()
                .get(&held(0).source_uri)
                .unwrap()
                .genres,
            ["folk", "last good jazz"]
        );
        let saved_progress = serde_json::to_vec(&first.genre_continuation).unwrap();
        store.close().unwrap();

        let previous: GenreContinuation = serde_json::from_slice(&saved_progress).unwrap();
        // A fresh full/playlist status must inherit the old cursor for this
        // same adoption, including when the previous pass fetched no artists.
        let mut discovered = GenreContinuation::default();
        discovered.owner.clone_from(&previous.owner);
        discovered.generation = previous.generation;
        discovered.pending = previous.pending;
        discovered.inherit_progress(&previous);
        let mut store = SqliteStore::open(&file).unwrap();
        let second = continue_with(&server, &cx, &mut store, Some(&discovered)).await;
        assert_eq!(second.genre_continuation.completed, 2);
        assert_eq!(second.genre_continuation.pending, GENRE_READ_BUDGET);
        assert_eq!(
            server.calls(ARTISTS)[GENRE_READ_BUDGET],
            route(GENRE_READ_BUDGET)
        );
        for number in GENRE_READ_BUDGET..count {
            assert_eq!(
                store.artist_genres(&artist_id(number)).unwrap().unwrap().0,
                ["blues"]
            );
        }
        assert_eq!(store.artist_genres(&artist_id(0)).unwrap(), saved);
        assert_eq!(store.library().unwrap(), accepted);
        store.close().unwrap();
    });
}

#[test]
fn cooldown_stops_requests_and_the_next_admitted_pass_resumes_after_the_failed_artist() {
    let server = Server::new();
    for number in 0..3 {
        tags(&server, number, &["soul"]);
    }
    server.state.lock().unwrap().refused.insert(route(0), 429);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        store.adopt_spotify_library_owner(OWNER).unwrap();
        apply_library_read(&mut store, &(0..3).map(held).collect::<Vec<_>>()).unwrap();
        store
            .save_artist_genres(&artist_id(0), &["last good jazz".into()], 0)
            .unwrap();
        let accepted = store.library().unwrap();
        let saved = store.artist_genres(&artist_id(0)).unwrap();
        let mut session = server.session(&cx, "user-library-read");
        let first = continue_genres(&mut session, &cx, &mut store, None)
            .await
            .unwrap();
        assert_eq!(first.genre_continuation.pending, 3);
        assert_eq!(first.genre_continuation.completed, 0);
        assert!(first.genre_continuation.retry_after_secs > 0);
        assert!(first.genres.halted.is_some());
        assert_eq!(server.calls(ARTISTS), [route(0)]);
        assert_eq!(store.artist_genres(&artist_id(0)).unwrap(), saved);
        let calls = server.calls("/v1/");
        let waiting = continue_genres(
            &mut session,
            &cx,
            &mut store,
            Some(&first.genre_continuation),
        )
        .await
        .unwrap();
        assert!(waiting.genre_continuation.retry_after_secs > 0);
        assert_eq!(
            server.calls("/v1/"),
            calls,
            "the identity read also respects cooldown"
        );
        assert_eq!(store.library().unwrap(), accepted);

        // The scheduler admits a fresh session after its durable not-before
        // deadline. Simulate that admission without a real hour-long wait.
        server.state.lock().unwrap().refused.remove(&route(0));
        let mut admitted = waiting.genre_continuation;
        admitted.retry_after_secs = 0;
        let done = continue_with(&server, &cx, &mut store, Some(&admitted)).await;
        assert_eq!(
            (
                done.genre_continuation.completed,
                done.genre_continuation.pending
            ),
            (3, 0)
        );
        assert_eq!(
            server.calls(ARTISTS),
            [route(0), route(1), route(2), route(0)]
        );
        assert_eq!(
            store.artist_genres(&artist_id(0)).unwrap().unwrap().0,
            ["soul"]
        );
        assert_eq!(
            pool_from_store(&store)
                .unwrap()
                .get(&held(0).source_uri)
                .unwrap()
                .genres,
            ["folk", "soul"],
            "new artist evidence preserves authoritative library tags"
        );
        assert_eq!(store.library().unwrap(), accepted);
    });
}

#[test]
fn ownership_changes_before_or_during_http_invalidate_saved_genre_progress() {
    for stage in ["before", "identity", "artist"] {
        let server = Server::new();
        tags(&server, 0, &["jazz"]);
        tags(&server, 1, &["jazz"]);
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let file = server.data.join("genre-owner.db");
            let mut store = SqliteStore::open(&file).unwrap();
            let (original, _) = store.adopt_spotify_library_owner(OWNER).unwrap();
            apply_library_read(&mut store, &[held(0), held(1)]).unwrap();
            let previous: GenreContinuation = serde_json::from_value(json!({
                "owner": OWNER, "generation": original.generation,
                "pending": 2, "cursor": artist_id(0)
            }))
            .unwrap();
            let switch = move || {
                let mut other = SqliteStore::open(&file).unwrap();
                other
                    .adopt_spotify_library_owner("another-test-owner")
                    .unwrap();
                let (returned, _) = other.adopt_spotify_library_owner(OWNER).unwrap();
                assert_ne!(returned.generation, original.generation);
                apply_library_read(&mut other, &[held(0), held(1)]).unwrap();
                other.close().unwrap();
            };
            if stage == "before" {
                switch();
            } else {
                server.state.lock().unwrap().before_response = Some((
                    if stage == "identity" {
                        "/v1/me".into()
                    } else {
                        route(1)
                    },
                    Box::new(switch),
                ));
            }
            let result = continue_with(&server, &cx, &mut store, Some(&previous)).await;
            assert!(result.genre_continuation.requires_refresh, "{stage}");
            assert_eq!(result.genre_continuation.completed, 0);
            assert!(store.artist_genres(&artist_id(1)).unwrap().is_none());
            assert_eq!(server.calls(ARTISTS).len(), usize::from(stage == "artist"));
            if stage == "before" {
                assert!(server.calls("/v1/").is_empty());
            }
            let accepted = store.library().unwrap();
            let rebuilt = continue_with(&server, &cx, &mut store, None).await;
            assert!(!rebuilt.genre_continuation.requires_refresh);
            assert_eq!(rebuilt.genre_continuation.completed, 2);
            assert_eq!(rebuilt.genre_continuation.pending, 0);
            assert_eq!(store.library().unwrap(), accepted);
            store.close().unwrap();
        });
    }
}

#[test]
fn a_different_signed_in_account_cannot_enrich_the_previous_owners_candidates() {
    let server = Server::new();
    server.set("/v1/me", json!({ "id": "another-test-owner" }));
    tags(&server, 0, &["jazz"]);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        let (owner, _) = store.adopt_spotify_library_owner(OWNER).unwrap();
        apply_library_read(&mut store, &[held(0)]).unwrap();
        let accepted = store.library().unwrap();
        let result = continue_with(&server, &cx, &mut store, None).await;
        assert!(result.genre_continuation.requires_refresh);
        assert!(server.calls(ARTISTS).is_empty());
        assert_eq!(store.spotify_library_owner().unwrap(), Some(owner));
        assert_eq!(store.library().unwrap(), accepted);
        assert!(store.artist_genres(&artist_id(0)).unwrap().is_none());
    });
}

#[test]
fn full_sync_reports_unfinished_artists_for_separate_continuation() {
    let server = Server::new();
    server.set(
        "/v1/me/tracks",
        page(
            (0..2)
                .map(|number| json!({ "track": api_track(number) }))
                .collect(),
            0,
            None,
        ),
    );
    for number in 0..2 {
        tags(&server, number, &["jazz"]);
    }
    server.state.lock().unwrap().refused.insert(route(0), 429);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        let mut session = server.session(&cx, "user-library-read");
        let first = sync_library_detailed(&mut session, &cx, &mut store)
            .await
            .unwrap();
        assert_eq!(first.genre_continuation.owner.as_deref(), Some(OWNER));
        assert_eq!(first.genre_continuation.pending, 2);
        assert!(first.genre_continuation.retry_after_secs > 0);
        let accepted = store.library().unwrap();
        assert!(accepted.iter().all(|row| row.artist_id.is_some()));
        let before = server.calls("/v1/").len();
        server.state.lock().unwrap().refused.remove(&route(0));
        let mut admitted = first.genre_continuation;
        admitted.retry_after_secs = 0;
        let done = continue_with(&server, &cx, &mut store, Some(&admitted)).await;
        assert_eq!(
            (
                done.genre_continuation.completed,
                done.genre_continuation.pending
            ),
            (2, 0)
        );
        assert_eq!(store.library().unwrap(), accepted);
        assert!(
            server.calls("/v1/")[before..]
                .iter()
                .all(|call| { path(call) == "/v1/me" || path(call).starts_with(ARTISTS) })
        );
    });
}

#[test]
fn playlist_continuation_discovers_new_artists_without_making_artist_requests() {
    let server = Server::new();
    server.set("/v1/me/playlists", page(vec![playlist(0, true)], 0, None));
    server.set(
        "/v1/playlists/TestList0/items",
        json!({
            "items": [{ "item": api_track(0) }, { "item": api_track(1) }],
            "offset": 0, "total": 2, "next": null
        }),
    );
    server
        .state
        .lock()
        .unwrap()
        .refused
        .insert("/v1/playlists/TestList0/items".into(), 429);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        let mut session = server.session(&cx, PLAYLIST_SCOPE);
        let first = sync_library_detailed(&mut session, &cx, &mut store)
            .await
            .unwrap();
        assert_eq!(first.playlists.pending, 1);
        assert_eq!(first.genre_continuation.pending, 0);
        server
            .state
            .lock()
            .unwrap()
            .refused
            .remove("/v1/playlists/TestList0/items");
        let mut session = server.session(&cx, PLAYLIST_SCOPE);
        let later = continue_playlists(&mut session, &cx, &mut store, OWNER)
            .await
            .unwrap();
        assert_eq!(later.playlists.pending, 0);
        assert_eq!(later.genre_continuation.pending, 2);
        assert_eq!(
            later.genre_continuation.owner,
            first.genre_continuation.owner
        );
        assert_eq!(
            later.genre_continuation.generation,
            first.genre_continuation.generation
        );
        assert!(
            store
                .library()
                .unwrap()
                .iter()
                .all(|row| row.artist_id.is_some())
        );
        assert!(server.calls(ARTISTS).is_empty());
    });
}

#[test]
fn genre_discovery_exposes_new_classical_works_for_album_completion() {
    let server = Server::new();
    tags(&server, 0, &["classical"]);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        store.adopt_spotify_library_owner(OWNER).unwrap();
        let mut item = held(0);
        item.album_uri = Some("spotify:album:GenreRecording".into());
        item.track_number = Some(2);
        apply_library_read(&mut store, std::slice::from_ref(&item)).unwrap();
        assert!(
            !works_from_store(&store)
                .unwrap()
                .work_of(&item.source_uri)
                .unwrap()
                .is_classical()
        );
        let result = continue_with(&server, &cx, &mut store, None).await;
        assert_eq!(result.library.classical, 1);
        assert_eq!(result.albums.pending, 1);
        assert!(
            works_from_store(&store)
                .unwrap()
                .work_of(&item.source_uri)
                .unwrap()
                .is_classical()
        );
        assert!(server.calls("/v1/albums/").is_empty());
    });
}

#[test]
fn missing_artist_ids_do_not_force_repeated_full_refreshes() {
    let server = Server::new();
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        let mut item = held(0);
        item.artist_id = None;
        apply_library_read(&mut store, std::slice::from_ref(&item)).unwrap();
        let legacy = continue_with(&server, &cx, &mut store, None).await;
        assert!(legacy.genre_continuation.requires_refresh);
        assert!(server.calls("/v1/").is_empty());
        store.adopt_spotify_library_owner(OWNER).unwrap();
        apply_library_read(&mut store, &[item]).unwrap();
        let result = continue_with(&server, &cx, &mut store, None).await;
        assert_eq!(result.genre_continuation.pending, 0);
        assert!(!result.genre_continuation.requires_refresh);
        assert!(server.calls("/v1/").is_empty());
    });
}
