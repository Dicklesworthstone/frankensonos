//! Full-sync account ownership through real HTTP and reopened SQLite caches.

use super::*;
use crate::cache::{LibrarySyncDetailed, continue_playlists, sync_library_detailed};
use fsonos_core::store::{CachedPlaylist, SqliteStore};

const FIRST: &str = "first-test-owner";
const SECOND: &str = "second-test-owner";
const OLD: usize = 90_000;
const SHARED: usize = 99_999;

fn uri(n: usize) -> String {
    format!("spotify:track:TestTrack{n}")
}

fn lists(server: &Server, owner: &str, ids: &[usize]) {
    server.set("/v1/me", json!({ "id": owner }));
    server.set(
        "/v1/me/playlists",
        page(
            ids.iter()
                .map(|n| {
                    json!({
                        "id": format!("Account{n:05}"), "name": format!("Playlist {n}"),
                        "owner": { "id": owner }, "snapshot_id": "test-version-1"
                    })
                })
                .collect(),
            0,
            None,
        ),
    );
    for n in ids {
        server.set(
            &format!("/v1/playlists/Account{n:05}/items"),
            page(vec![json!({ "item": track(*n) })], 0, None),
        );
    }
}

fn liked(server: &Server, n: usize) {
    server.set(
        "/v1/me/tracks",
        page(vec![json!({ "track": track(n) })], 0, None),
    );
}

async fn sync(server: &Server, cx: &Cx, store: &mut dyn Store) -> LibrarySyncDetailed {
    let mut session = server.session(cx, PLAYLIST_SCOPE);
    sync_library_detailed(&mut session, cx, store)
        .await
        .unwrap()
}

async fn seed_old_account(
    server: &Server,
    cx: &Cx,
    file: &std::path::Path,
) -> Option<CachedPlaylist> {
    let mut store = SqliteStore::open(file).unwrap();
    sync(server, cx, &mut store).await;
    let observation = store
        .playlist_contents(&format!("spotify:playlist:Account{OLD:05}"))
        .unwrap();
    assert!(observation.is_some());
    let mut shared = store
        .library()
        .unwrap()
        .into_iter()
        .find(|entry| entry.track.source_uri == uri(SHARED))
        .unwrap();
    shared.taste_weight = Some(1136);
    store.upsert_library(&[shared]).unwrap();
    store.save_spotify_heard(&[(uri(OLD), 100)]).unwrap();
    store
        .save_artist_genres("PublicArtist", &["jazz".into()], 100)
        .unwrap();
    store.close().unwrap();
    observation
}

#[test]
fn same_owner_partial_reads_keep_taste_but_a_new_owner_never_inherits_it_after_restart() {
    let server = Server::new();
    lists(&server, FIRST, &[OLD]);
    liked(&server, SHARED);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let file = server.data.join("account-ownership.db");
        let old_playlist = format!("spotify:playlist:Account{OLD:05}");
        let observation = seed_old_account(&server, &cx, &file).await;

        // Optional failure on the same proved account retains its prior
        // candidates, their account weights, and last-good observations.
        server.set("/v1/me/playlists", json!({ "items": "bad directory" }));
        let mut store = SqliteStore::open(&file).unwrap();
        let partial = sync(&server, &cx, &mut store).await;
        assert_eq!(partial.library.retired, 0);
        assert!(pool_from_store(&store).unwrap().get(&uri(OLD)).is_some());
        assert_eq!(store.playlist_contents(&old_playlist).unwrap(), observation);
        assert_eq!(store.spotify_heard().unwrap(), [(uri(OLD), 100)]);
        assert_eq!(
            store
                .library()
                .unwrap()
                .into_iter()
                .find(|entry| entry.track.source_uri == uri(SHARED))
                .unwrap()
                .taste_weight,
            Some(1136)
        );
        store.close().unwrap();

        // Thirty current playlists exceed a full pass's twenty-five-window
        // budget. The incomplete-read preservation branch must retain only
        // this new owner's accepted evidence, including a shared saved track.
        lists(&server, SECOND, &(0..30).collect::<Vec<_>>());
        let mut store = SqliteStore::open(&file).unwrap();
        let changed = sync(&server, &cx, &mut store).await;
        assert_eq!(changed.playlists.owner.as_deref(), Some(SECOND));
        assert_eq!(changed.playlists.pending, 5);
        assert!(
            changed
                .notes
                .iter()
                .any(|note| note.contains("playlist budget"))
        );
        assert_eq!(changed.library.retired, 1);
        assert_eq!(
            store
                .spotify_library_owner()
                .unwrap()
                .map(|owner| owner.owner_id)
                .as_deref(),
            Some(SECOND)
        );
        assert!(pool_from_store(&store).unwrap().get(&uri(OLD)).is_none());
        assert!(pool_from_store(&store).unwrap().get(&uri(0)).is_some());
        assert!(store.playlist_contents(&old_playlist).unwrap().is_none());
        assert!(store.spotify_heard().unwrap().is_empty());
        assert!(store.artist_genres("PublicArtist").unwrap().is_some());
        let common = store
            .library()
            .unwrap()
            .into_iter()
            .find(|entry| entry.track.source_uri == uri(SHARED))
            .unwrap();
        assert!(common.is_candidate());
        assert_eq!(
            common.taste_weight, None,
            "the prior account's weight cannot leak"
        );
        store.close().unwrap();

        let mut store = SqliteStore::open(&file).unwrap();
        assert_eq!(
            store
                .spotify_library_owner()
                .unwrap()
                .map(|owner| owner.owner_id)
                .as_deref(),
            Some(SECOND)
        );
        let before = server.calls("/v1/").len();
        let mut stale = server.session(&cx, PLAYLIST_SCOPE);
        let refused = continue_playlists(&mut stale, &cx, &mut store, FIRST)
            .await
            .unwrap();
        assert!(refused.playlists.requires_refresh);
        assert_eq!(
            server.calls("/v1/").len(),
            before,
            "a stale scheduler owner sends no read"
        );
        let mut session = server.session(&cx, PLAYLIST_SCOPE);
        let completed = continue_playlists(&mut session, &cx, &mut store, SECOND)
            .await
            .unwrap();
        assert_eq!(completed.playlists.pending, 0);
        assert_eq!(completed.library.tracks, 31);
        assert!(pool_from_store(&store).unwrap().get(&uri(29)).is_some());
        assert!(pool_from_store(&store).unwrap().get(&uri(OLD)).is_none());
        store.close().unwrap();
    });
}

#[test]
fn owner_identity_and_the_complete_saved_library_are_required_before_adoption() {
    let server = Server::new();
    lists(&server, FIRST, &[OLD]);
    liked(&server, SHARED);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        sync(&server, &cx, &mut store).await;
        let before = store.library().unwrap();
        let playlists = store.playlists().unwrap();
        let old_playlist = format!("spotify:playlist:Account{OLD:05}");
        let observation = store.playlist_contents(&old_playlist).unwrap();

        for profile in [json!({}), json!({ "id": "" }), json!({ "id": "owner\n" })] {
            server.set("/v1/me", profile);
            let calls = server.calls("/v1/").len();
            let mut session = server.session(&cx, PLAYLIST_SCOPE);
            assert!(
                sync_library_detailed(&mut session, &cx, &mut store)
                    .await
                    .is_err()
            );
            assert!(
                server.calls("/v1/")[calls..]
                    .iter()
                    .all(|call| path(call) == "/v1/me")
            );
            assert_eq!(
                store
                    .spotify_library_owner()
                    .unwrap()
                    .map(|owner| owner.owner_id)
                    .as_deref(),
                Some(FIRST)
            );
            assert_eq!(store.library().unwrap(), before);
            assert_eq!(store.playlists().unwrap(), playlists);
        }

        server.set("/v1/me", json!({ "id": SECOND }));
        server.set(
            "/v1/me/tracks",
            json!({ "items": "bad saved-library page" }),
        );
        let mut session = server.session(&cx, PLAYLIST_SCOPE);
        assert!(
            sync_library_detailed(&mut session, &cx, &mut store)
                .await
                .is_err()
        );
        assert_eq!(
            store
                .spotify_library_owner()
                .unwrap()
                .map(|owner| owner.owner_id)
                .as_deref(),
            Some(FIRST)
        );
        assert_eq!(store.library().unwrap(), before);
        assert_eq!(store.playlists().unwrap(), playlists);
        assert_eq!(store.playlist_contents(&old_playlist).unwrap(), observation);
    });
}

#[test]
fn library_only_grants_verify_the_owner_and_unowned_legacy_taste_is_not_adopted() {
    let server = Server::new();
    liked(&server, SHARED);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        for previous in [None, Some(FIRST)] {
            let mut store = MemStore::default();
            if let Some(owner) = previous {
                store.adopt_spotify_library_owner(owner).unwrap();
            }
            apply_library_read(
                &mut store,
                &[LibraryItem {
                    source_uri: uri(OLD),
                    title: "Old account song".into(),
                    artists: vec!["Test performer".into()],
                    origin: Origin::Taste,
                    taste_pm: Some(1136),
                    ..LibraryItem::default()
                }],
            )
            .unwrap();
            store.save_spotify_heard(&[(uri(OLD), 100)]).unwrap();
            server.set("/v1/me", json!({ "id": SECOND }));
            let before = server.calls("/v1/").len();
            let mut session = server.session(&cx, "user-library-read");
            let read = sync_library_detailed(&mut session, &cx, &mut store)
                .await
                .unwrap();
            assert_eq!(read.playlists.owner.as_deref(), Some(SECOND));
            assert_eq!(read.playlists.pending, 0);
            assert_eq!(
                store
                    .spotify_library_owner()
                    .unwrap()
                    .map(|owner| owner.owner_id)
                    .as_deref(),
                Some(SECOND)
            );
            assert_eq!(read.library.retired, 1);
            assert_eq!(read.library.tracks, 1);
            assert!(pool_from_store(&store).unwrap().get(&uri(OLD)).is_none());
            assert!(pool_from_store(&store).unwrap().get(&uri(SHARED)).is_some());
            assert!(store.spotify_heard().unwrap().is_empty());
            assert!(
                server.calls("/v1/")[before..]
                    .iter()
                    .any(|call| path(call) == "/v1/me")
            );
            assert!(server.calls("/v1/")[before..].iter().all(|call| {
                !path(call).contains("/playlists") && !path(call).contains("/top/")
            }));
        }
    });
}

fn switch_during_next_playlist(server: &Server, file: PathBuf, returning: bool) {
    server.state.lock().unwrap().before_response = Some((
        "/v1/playlists/Account00025/items".into(),
        Box::new(move || {
            let mut competing = SqliteStore::open(&file).unwrap();
            let (second, _) = competing.adopt_spotify_library_owner(SECOND).unwrap();
            let owner = if returning {
                competing.adopt_spotify_library_owner(FIRST).unwrap().0
            } else {
                second
            };
            let current = LibraryItem {
                source_uri: uri(SHARED),
                title: "New adoption's song".into(),
                artists: vec!["Test performer".into()],
                origin: Origin::LikedTrack,
                ..LibraryItem::default()
            };
            let pool = crate::classical::CandidatePool::build(std::slice::from_ref(&current));
            let entry = crate::cache::to_entry(&current, pool.get(&current.source_uri));
            assert!(
                competing
                    .publish_spotify_library(
                        &owner,
                        &[entry],
                        Some(&[(
                            "spotify:playlist:CurrentList".into(),
                            "Current account".into()
                        )]),
                        &[CachedPlaylist::new(
                            "spotify:playlist:CurrentList".into(),
                            123,
                            Some(0),
                            0,
                            true,
                            Vec::new()
                        )],
                        Some(&[(uri(SHARED), 123)]),
                    )
                    .unwrap()
            );
            competing.close().unwrap();
        }),
    ));
}

#[test]
fn continuation_cannot_publish_after_an_account_switch_during_http_even_when_that_owner_returns() {
    for returning in [false, true] {
        let server = Server::new();
        lists(&server, FIRST, &(0..30).collect::<Vec<_>>());
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let file = server.data.join("continuation-account-race.db");
            let mut store = SqliteStore::open(&file).unwrap();
            let initial = sync(&server, &cx, &mut store).await;
            assert_eq!(initial.playlists.pending, 5);
            let original = store.spotify_library_owner().unwrap().unwrap();
            switch_during_next_playlist(&server, file.clone(), returning);
            let before = server.item_calls().len();
            let mut session = server.session(&cx, PLAYLIST_SCOPE);
            let continuation = continue_playlists(&mut session, &cx, &mut store, FIRST)
                .await
                .unwrap();
            assert!(
                server.item_calls().len() > before,
                "the race happens during a real HTTP read"
            );
            assert!(server.state.lock().unwrap().before_response.is_none());
            assert!(continuation.playlists.requires_refresh);
            let owner = store.spotify_library_owner().unwrap().unwrap();
            assert_eq!(owner.owner_id, if returning { FIRST } else { SECOND });
            assert_eq!(
                owner.generation,
                original.generation + if returning { 2 } else { 1 }
            );
            assert_eq!(continuation.library.tracks, 1);
            assert!(pool_from_store(&store).unwrap().get(&uri(SHARED)).is_some());
            assert!(pool_from_store(&store).unwrap().get(&uri(25)).is_none());
            assert_eq!(
                store.playlists().unwrap(),
                [(
                    "spotify:playlist:CurrentList".into(),
                    "Current account".into()
                )]
            );
            assert_eq!(store.spotify_heard().unwrap(), [(uri(SHARED), 123)]);
            let pending = store
                .playlist_progress(FIRST, "spotify:playlist:Account00025")
                .unwrap()
                .unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&pending).unwrap()["applied"],
                false
            );
            store.close().unwrap();
            let reopened = SqliteStore::open(&file).unwrap();
            assert_eq!(reopened.spotify_library_owner().unwrap(), Some(owner));
            assert!(pool_from_store(&reopened).unwrap().get(&uri(25)).is_none());
            assert!(
                reopened
                    .playlist_contents("spotify:playlist:CurrentList")
                    .unwrap()
                    .is_some()
            );
            reopened.close().unwrap();
        });
    }
}
