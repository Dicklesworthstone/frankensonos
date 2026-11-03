//! Long playlists progress across real store reopenings without mixing versions.

use super::*;
use crate::cache::{LibrarySyncDetailed, sync_library_detailed};
use fsonos_core::store::{CachedPlaylist, MAX_PLAYLIST_ENTRIES, SqliteStore};

const ID: &str = "LongList";
const URI: &str = "spotify:playlist:LongList";
const OWNER: &str = "test-owner";

fn directory(server: &Server, snapshot: Option<&str>, owner: &str) {
    let mut list = json!({ "id": ID, "name": "Long playlist", "owner": { "id": owner } });
    if let Some(snapshot) = snapshot {
        list["snapshot_id"] = snapshot.into();
    }
    server.set("/v1/me", json!({ "id": owner }));
    server.set("/v1/me/playlists", page(vec![list], 0, None));
}

fn snapshot(server: &Server, version: &str) {
    server.set(
        &format!("/v1/playlists/{ID}"),
        json!({ "id": ID, "snapshot_id": version }),
    );
}

fn route(offset: usize) -> String {
    format!("/v1/playlists/{ID}/items?offset={offset}")
}

fn populate(server: &Server, version: &str, count: usize, base: usize) {
    directory(server, Some(version), OWNER);
    snapshot(server, version);
    for offset in (0..count).step_by(50) {
        let end = (offset + 50).min(count);
        let items = (offset..end)
            .map(|n| json!({ "item": track(base + n) }))
            .collect::<Vec<_>>();
        server.set(
            &route(offset),
            json!({
                "items": items, "offset": offset, "total": count,
                "next": (end < count).then(|| format!("{API}/playlists/{ID}/items?offset={end}"))
            }),
        );
    }
}

async fn sync(server: &Server, cx: &Cx, store: &mut dyn Store) -> LibrarySyncDetailed {
    let mut session = server.session(cx, PLAYLIST_SCOPE);
    sync_library_detailed(&mut session, cx, store)
        .await
        .unwrap()
}

fn contents(store: &dyn Store) -> CachedPlaylist {
    store.playlist_contents(URI).unwrap().unwrap()
}

fn checkpoint(store: &dyn Store, owner: &str) -> String {
    store.playlist_progress(owner, URI).unwrap().unwrap()
}

fn offsets(server: &Server) -> Vec<u32> {
    server
        .item_calls()
        .iter()
        .map(|call| parameter(call, "offset").unwrap().parse().unwrap())
        .collect()
}

#[test]
fn long_playlist_completes_across_restarts_and_all_occurrences_reach_the_library() {
    let server = Server::new();
    populate(&server, "version-one", 405, 0);
    {
        let mut script = server.state.lock().unwrap();
        script.pages.get_mut(&route(100)).unwrap()["items"][49] = Value::Null;
        script.pages.get_mut(&route(150)).unwrap()["items"][0] = json!({ "item": track(0) });
        script.pages.get_mut(&route(250)).unwrap()["items"][49] =
            json!({ "item": { "type": "episode", "uri": "spotify:episode:Skipped" } });
    }
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let file = server.data.join("long-playlist.db");
        for examined in [150, 300, 405] {
            let mut store = SqliteStore::open(&file).unwrap();
            let before = server.calls("/v1/playlists/").len();
            let result = sync(&server, &cx, &mut store).await;
            assert_eq!(server.calls("/v1/playlists/").len() - before, 4);
            let observed = contents(&store);
            assert_eq!(observed.examined, examined);
            assert_eq!(observed.source_total, Some(405));
            assert_eq!(observed.complete, examined == 405);
            assert_eq!(
                result
                    .notes
                    .iter()
                    .any(|note| note.contains("taste signals not read")),
                examined != 405,
                "{result:?}",
            );
            store.close().unwrap();
        }
        let store = SqliteStore::open(&file).unwrap();
        let observed = contents(&store);
        assert_eq!(observed.tracks.len(), 403);
        assert!(
            !observed
                .tracks
                .iter()
                .any(|entry| [150, 300].contains(&entry.position))
        );
        let duplicate = observed
            .tracks
            .iter()
            .find(|entry| entry.position == 151)
            .unwrap();
        assert_eq!(duplicate.source_uri, observed.tracks[0].source_uri);
        assert_eq!(observed.tracks.last().unwrap().position, 405);
        let pool = pool_from_store(&store).unwrap();
        assert_eq!(
            pool.len(),
            402,
            "only the DJ/library projection deduplicates"
        );
        assert!(pool.get("spotify:track:TestTrack404").is_some());
        let saved: Value = serde_json::from_str(&checkpoint(&store, OWNER)).unwrap();
        assert_eq!(saved["complete"], true);
        assert_eq!(saved["snapshot_id"], "version-one");
        store.close().unwrap();
    });
    assert_eq!(offsets(&server), [0, 50, 100, 150, 200, 250, 300, 350, 400]);
}

#[test]
fn changed_versions_restart_without_hiding_the_last_complete_playlist() {
    let server = Server::new();
    populate(&server, "original", 3, 0);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        sync(&server, &cx, &mut store).await;
        let original = contents(&store);
        populate(&server, "replacement-one", 350, 1_000);
        for examined in [150, 300] {
            sync(&server, &cx, &mut store).await;
            assert_eq!(
                contents(&store),
                original,
                "published revision stays stable while assembling"
            );
            let progress: Value = serde_json::from_str(&checkpoint(&store, OWNER)).unwrap();
            assert_eq!(progress["examined"], examined);
        }
        // Same length, different version and order: count checks alone would
        // wrongly join the first 300 old entries to 50 new entries.
        populate(&server, "replacement-two", 350, 2_000);
        sync(&server, &cx, &mut store).await;
        let progress: Value = serde_json::from_str(&checkpoint(&store, OWNER)).unwrap();
        assert_eq!(progress["examined"], 150);
        assert_eq!(
            progress["tracks"][0]["item"]["source_uri"],
            "spotify:track:TestTrack2000"
        );
        assert_eq!(contents(&store), original);
        sync(&server, &cx, &mut store).await;
        assert_eq!(contents(&store), original);
        sync(&server, &cx, &mut store).await;
        let replaced = contents(&store);
        assert!(replaced.complete);
        assert_eq!(replaced.tracks.len(), 350);
        assert_eq!(replaced.tracks[0].source_uri, "spotify:track:TestTrack2000");
        assert_eq!(
            replaced.tracks[349].source_uri,
            "spotify:track:TestTrack2349"
        );
        assert_ne!(replaced.revision, original.revision);
        let pool = pool_from_store(&store).unwrap();
        assert_eq!(pool.len(), 350);
        assert!(pool.get("spotify:track:TestTrack1000").is_none());
        assert!(pool.get("spotify:track:TestTrack2349").is_some());
    });
    assert_eq!(
        offsets(&server),
        [0, 0, 50, 100, 150, 200, 250, 0, 50, 100, 150, 200, 250, 300]
    );
}

#[test]
fn failed_snapshot_proofs_preserve_the_checkpoint_cache_and_taste() {
    let server = Server::new();
    populate(&server, "stable", 400, 0);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        sync(&server, &cx, &mut store).await;
        let original = contents(&store);
        let saved = checkpoint(&store, OWNER);
        for proof in [
            json!({ "id": ID, "snapshot_id": "changed-during-the-read" }),
            json!({ "id": "OtherPlaylist", "snapshot_id": "stable" }),
            json!({ "id": ID, "snapshot_id": "" }),
            json!({ "id": ID }),
        ] {
            server.set(&format!("/v1/playlists/{ID}"), proof);
            let before = server.calls("/v1/playlists/").len();
            let result = sync(&server, &cx, &mut store).await;
            assert!(!result.notes.is_empty());
            assert_eq!(server.calls("/v1/playlists/").len() - before, 4);
            assert_eq!(checkpoint(&store, OWNER), saved);
            assert_eq!(contents(&store), original);
            assert_eq!(pool_from_store(&store).unwrap().len(), 150);
        }
        snapshot(&server, "stable");
        server
            .state
            .lock()
            .unwrap()
            .refused
            .insert(format!("/v1/playlists/{ID}"), 429);
        let limited = sync(&server, &cx, &mut store).await;
        assert!(limited.notes.iter().any(|note| note.contains("429")));
        assert_eq!(checkpoint(&store, OWNER), saved);
        assert_eq!(contents(&store), original);
        server
            .state
            .lock()
            .unwrap()
            .refused
            .remove(&format!("/v1/playlists/{ID}"));
        sync(&server, &cx, &mut store).await;
        assert_eq!(contents(&store).examined, 300);
        assert_eq!(pool_from_store(&store).unwrap().len(), 300);
    });
}

#[test]
fn a_malformed_resumed_window_does_not_advance_the_durable_checkpoint() {
    let server = Server::new();
    populate(&server, "same-version", 350, 0);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        sync(&server, &cx, &mut store).await;
        let original = contents(&store);
        let saved = checkpoint(&store, OWNER);
        server.set(
            &route(200),
            json!({
                "items": [{ "item": track(200) }], "offset": 200,
                "total": 351, "next": format!("{API}/playlists/{ID}/items?offset=201")
            }),
        );
        let before = server.item_calls().len();
        let failed = sync(&server, &cx, &mut store).await;
        assert!(
            failed
                .notes
                .iter()
                .any(|note| note.contains("changed total"))
        );
        assert_eq!(&offsets(&server)[before..], &[150, 200]);
        assert_eq!(checkpoint(&store, OWNER), saved);
        assert_eq!(contents(&store), original);
        // A corrected retry rereads the previous window's accepted first page.
        populate(&server, "same-version", 350, 0);
        let before = server.item_calls().len();
        sync(&server, &cx, &mut store).await;
        assert_eq!(&offsets(&server)[before..], &[150, 200, 250]);
        assert_eq!(contents(&store).examined, 300);
        sync(&server, &cx, &mut store).await;
        assert!(contents(&store).complete);
        assert_eq!(contents(&store).tracks.len(), 350);
    });
}

#[test]
fn account_changes_and_deleted_playlists_cannot_reuse_another_read_checkpoint() {
    let server = Server::new();
    populate(&server, "shared-version", 400, 0);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        sync(&server, &cx, &mut store).await;
        let first_owner = checkpoint(&store, OWNER);
        // A second authorized collaborator reads the same playlist/version.
        server.set("/v1/me", json!({ "id": "other-owner" }));
        server.set(
            "/v1/me/playlists",
            page(
                vec![json!({
                    "id": ID, "name": "Shared", "owner": { "id": OWNER },
                    "collaborative": true, "snapshot_id": "shared-version"
                })],
                0,
                None,
            ),
        );
        sync(&server, &cx, &mut store).await;
        assert_eq!(offsets(&server), [0, 50, 100, 0, 50, 100]);
        assert_eq!(checkpoint(&store, OWNER), first_owner);
        let second_owner = checkpoint(&store, "other-owner");
        server.set("/v1/me", json!({ "id": OWNER }));
        // Returning to an account discards its old applied proof. A broken
        // optional directory cannot label that discarded observation current.
        server.set(
            "/v1/me/playlists",
            json!({ "items": [], "offset": 0, "total": 1,
            "next": format!("{API}/me/playlists?offset=1") }),
        );
        sync(&server, &cx, &mut store).await;
        assert_eq!(store.playlist_progress(OWNER, URI).unwrap(), None);
        server.set("/v1/me/playlists", page(Vec::new(), 0, None));
        sync(&server, &cx, &mut store).await;
        assert_eq!(store.playlist_progress(OWNER, URI).unwrap(), None);
        assert_eq!(checkpoint(&store, "other-owner"), second_owner);
        assert!(pool_from_store(&store).unwrap().is_empty());
        // Returning to the directory starts at zero, with no resurrected suffix.
        populate(&server, "shared-version", 400, 0);
        let before = server.item_calls().len();
        sync(&server, &cx, &mut store).await;
        assert_eq!(&offsets(&server)[before..], &[0, 50, 100]);
        server.set("/v1/me", json!({ "id": "" }));
        let mut session = server.session(&cx, PLAYLIST_SCOPE);
        let (taste, read) = read_taste(&mut session, &cx, &mut store, &[], 1).await;
        assert!(taste.tracks.is_empty() && read.incomplete());
        assert_eq!(store.playlist_progress("", URI).unwrap(), None);
    });
}

#[test]
fn a_missing_directory_version_spends_the_same_request_budget_on_a_preflight_check() {
    let server = Server::new();
    populate(&server, "preflight-version", 225, 0);
    directory(&server, None, OWNER);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        for examined in [100, 200, 225] {
            let before = server.calls("/v1/playlists/").len();
            sync(&server, &cx, &mut store).await;
            assert!(server.calls("/v1/playlists/").len() - before <= 4);
            assert_eq!(contents(&store).examined, examined);
        }
        assert!(contents(&store).complete);
        assert_eq!(pool_from_store(&store).unwrap().len(), 225);
    });
    assert_eq!(offsets(&server), [0, 50, 100, 150, 200]);
}

#[test]
fn the_raw_entry_limit_is_partial_and_never_fetches_past_its_bound() {
    let server = Server::new();
    directory(&server, Some("large"), OWNER);
    snapshot(&server, "large");
    let first = MAX_PLAYLIST_ENTRIES - 50;
    server.set(&route(usize::try_from(first).unwrap()), json!({
        "items": (first..MAX_PLAYLIST_ENTRIES).map(|n| json!({ "item": track(usize::try_from(n).unwrap()) })).collect::<Vec<_>>(),
        "offset": first, "total": MAX_PLAYLIST_ENTRIES + 1,
        "next": format!("{API}/playlists/{ID}/items?offset={MAX_PLAYLIST_ENTRIES}")
    }));
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        store.adopt_spotify_library_owner(OWNER).unwrap();
        store
            .save_playlist_progress(
                OWNER,
                URI,
                &json!({
                    "version": 1, "owner_id": OWNER, "playlist_uri": URI, "snapshot_id": "large",
                    "source_total": MAX_PLAYLIST_ENTRIES + 1, "examined": first,
                    "complete": false, "tracks": []
                })
                .to_string(),
            )
            .unwrap();
        let result = sync(&server, &cx, &mut store).await;
        assert!(
            result
                .notes
                .iter()
                .any(|note| note.contains("10,000-entry"))
        );
        let observed = contents(&store);
        assert_eq!(observed.examined, MAX_PLAYLIST_ENTRIES);
        assert!(!observed.complete);
        assert_eq!(observed.tracks.len(), 50);
        assert_eq!(offsets(&server), [first]);
        sync(&server, &cx, &mut store).await;
        assert_eq!(
            offsets(&server),
            [first],
            "only a snapshot check is needed at the cap"
        );
        assert!(!contents(&store).complete);
    });
}
