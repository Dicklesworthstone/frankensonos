//! A completed large directory must support one authoritative full taste union.

use super::*;
use crate::taste::{MAX_PLAYLIST_REQUESTS, MAX_PLAYLISTS};

const TOTAL: usize = WINDOW + 51;
const FIRST_OWNED: usize = 75;
const OWNED: usize = 35;
const OBSOLETE: &str = "spotify:track:ObsoleteDirectoryTaste";

fn middle_owned_directory(server: &Server) {
    populate(server, TOTAL, 0);
    {
        let mut state = server.state.lock().unwrap();
        for n in FIRST_OWNED..FIRST_OWNED + OWNED {
            let offset = n / PAGE_LIMIT as usize * PAGE_LIMIT as usize;
            let route = format!("/v1/me/playlists?offset={offset}");
            state.pages.get_mut(&route).unwrap()["items"][n - offset]["owner"] =
                json!({ "id": OWNER });
        }
        let offset = (TOTAL - 1) / PAGE_LIMIT as usize * PAGE_LIMIT as usize;
        let route = format!("/v1/me/playlists?offset={offset}");
        state.pages.get_mut(&route).unwrap()["items"][TOTAL - 1 - offset]["owner"] =
            json!({ "id": "another-owner" });
    }
    for n in FIRST_OWNED..FIRST_OWNED + OWNED {
        server.set(
            &format!("/v1/playlists/{}/items", id(n)),
            page(vec![json!({ "item": track(n) })], 0, None),
        );
    }
}

fn seed_obsolete_taste(store: &mut dyn Store) -> SpotifyLibraryOwner {
    let owner = seed(store);
    apply_library_read(
        store,
        &[LibraryItem {
            source_uri: OBSOLETE.into(),
            title: "Old taste song".into(),
            artists: vec!["Test performer".into()],
            origin: Origin::Taste,
            ..LibraryItem::default()
        }],
    )
    .unwrap();
    owner
}

fn edit_middle_playlist(server: &Server, n: usize, replacement: usize) {
    let offset = n / PAGE_LIMIT as usize * PAGE_LIMIT as usize;
    let route = format!("/v1/me/playlists?offset={offset}");
    server.state.lock().unwrap().pages.get_mut(&route).unwrap()["items"][n - offset]["snapshot_id"] =
        json!("edited-during-continuation");
    server.set(
        &format!("/v1/playlists/{}", id(n)),
        json!({ "id": id(n), "snapshot_id": "edited-during-continuation" }),
    );
    server.set(
        &format!("/v1/playlists/{}/items", id(n)),
        page(vec![json!({ "item": track(replacement) })], 0, None),
    );
}

#[test]
fn many_owned_middle_playlists_reconcile_once_after_bounded_completion_and_restart() {
    let server = Server::new();
    middle_owned_directory(&server);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let file = server.data.join("middle-directory-reconciliation.db");
        let mut store = SqliteStore::open(&file).unwrap();
        let owner = seed_obsolete_taste(&mut store);
        let first = full(&server, &cx, &mut store).await;
        assert_eq!(first.playlists.pending, 1 + OWNED - MAX_PLAYLISTS);
        assert_eq!(first.library.retired, 0);
        assert_eq!(server.calls("/v1/me/playlists").len(), MAX_PAGES);
        assert_eq!(server.item_calls().len(), MAX_PLAYLISTS);
        assert_eq!(checkpoint(&store, &owner)["examined"], WINDOW);
        assert!(pool_from_store(&store).unwrap().get(OBSOLETE).is_some());
        store.close().unwrap();

        // This item is outside both rechecked directory boundaries and the
        // first full pass's 25 playlist reads. Its pending version changes.
        let edited = FIRST_OWNED + 30;
        let replacement = 10_000 + edited;
        edit_middle_playlist(&server, edited, replacement);
        let mut store = SqliteStore::open(&file).unwrap();
        let directory_reads = server.calls("/v1/me/playlists").len();
        let playlist_reads = server.calls("/v1/playlists/").len();
        let completed = pending(&server, &cx, &mut store).await;
        assert_eq!(completed.playlists.pending, 0);
        assert_eq!(completed.library.retired, 0);
        assert!(server.calls("/v1/me/playlists").len() - directory_reads <= MAX_PAGES);
        assert!(
            server.calls("/v1/playlists/").len() - playlist_reads
                <= MAX_PLAYLISTS * MAX_PLAYLIST_REQUESTS
        );
        assert_eq!(checkpoint(&store, &owner)["examined"], TOTAL);
        assert_eq!(checkpoint(&store, &owner)["reconcile"], true);
        assert_eq!(store.playlists().unwrap().len(), TOTAL - 1);
        assert_eq!(pool_from_store(&store).unwrap().len(), OWNED + 1);
        assert!(pool_from_store(&store).unwrap().get(OBSOLETE).is_some());
        let contents = store.playlist_contents(&uri(edited)).unwrap().unwrap();
        assert!(contents.complete);
        assert_eq!(
            contents.tracks[0].source_uri,
            format!("spotify:track:TestTrack{replacement}")
        );
        store.close().unwrap();

        let mut store = SqliteStore::open(&file).unwrap();
        let directory_reads = server.calls("/v1/me/playlists").len();
        let item_reads = server.item_calls().len();
        let reconciled = full(&server, &cx, &mut store).await;
        assert_eq!(reconciled.playlists.pending, 0);
        assert_eq!(reconciled.library.tracks, OWNED);
        assert_eq!(reconciled.library.retired, 1);
        assert_eq!(server.calls("/v1/me/playlists").len() - directory_reads, 2);
        let calls = server.item_calls();
        assert_eq!(calls.len() - item_reads, MAX_PLAYLISTS);
        let edited_route = format!("/v1/playlists/{}/items", id(edited));
        assert!(
            calls[item_reads..]
                .iter()
                .all(|call| path(call) != edited_route),
            "the changed middle version must survive through its accepted proof, not a new read"
        );
        assert_eq!(checkpoint(&store, &owner)["reconcile"], false);
        let pool = pool_from_store(&store).unwrap();
        assert_eq!(pool.len(), OWNED);
        assert!(pool.get(OBSOLETE).is_none());
        for n in FIRST_OWNED..FIRST_OWNED + OWNED {
            let expected = if n == edited { replacement } else { n };
            assert!(
                pool.get(&format!("spotify:track:TestTrack{expected}"))
                    .is_some(),
                "unchanged, unvisited middle playlists must remain candidates"
            );
        }
        assert!(
            pool.get(&format!("spotify:track:TestTrack{edited}"))
                .is_none()
        );
        store.close().unwrap();

        // Ordinary continuation must not recreate a consumed receipt or
        // reopen a completed scan just because the daemon restarted.
        let mut store = SqliteStore::open(&file).unwrap();
        let directory_reads = server.calls("/v1/me/playlists").len();
        let item_reads = server.item_calls().len();
        let idle = pending(&server, &cx, &mut store).await;
        assert_eq!((idle.playlists.pending, idle.playlists.advanced), (0, 0));
        assert_eq!(server.calls("/v1/me/playlists").len() - directory_reads, 2);
        assert_eq!(server.item_calls().len(), item_reads);
        assert_eq!(checkpoint(&store, &owner)["reconcile"], false);
        store.close().unwrap();

        // The receipt grants one union; a later explicit full refresh must
        // discover fresh middle-page membership rather than reuse it forever.
        let mut store = SqliteStore::open(&file).unwrap();
        let directory_reads = server.calls("/v1/me/playlists").len();
        let refreshed = full(&server, &cx, &mut store).await;
        assert_eq!(
            server.calls("/v1/me/playlists").len() - directory_reads,
            MAX_PAGES
        );
        assert_eq!(refreshed.playlists.pending, 1);
        assert_eq!(refreshed.library.retired, 0);
        assert_eq!(checkpoint(&store, &owner)["examined"], WINDOW);
        assert_eq!(checkpoint(&store, &owner)["complete"], false);
        assert_eq!(store.playlists().unwrap().len(), TOTAL - 1);
        assert_eq!(pool_from_store(&store).unwrap().len(), OWNED);
        store.close().unwrap();
    });
}

#[test]
fn failed_and_incomplete_full_refreshes_preserve_the_completed_directory_receipt() {
    let server = Server::new();
    middle_owned_directory(&server);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let file = server.data.join("middle-directory-reconciliation-retry.db");
        let mut store = SqliteStore::open(&file).unwrap();
        let owner = seed_obsolete_taste(&mut store);
        assert!(full(&server, &cx, &mut store).await.playlists.pending > 0);
        store.close().unwrap();
        let mut store = SqliteStore::open(&file).unwrap();
        assert_eq!(pending(&server, &cx, &mut store).await.playlists.pending, 0);
        assert_eq!(checkpoint(&store, &owner)["reconcile"], true);
        store.close().unwrap();

        let mut store = SqliteStore::open(&file).unwrap();
        let accepted = store.spotify_playlist_directory(&owner).unwrap();
        let directory_reads = server.calls("/v1/me/playlists").len();
        server.set("/v1/me/tracks", json!({ "not_a_page": true }));
        let mut session = server.session(&cx, PLAYLIST_SCOPE);
        assert!(
            sync_library_detailed(&mut session, &cx, &mut store)
                .await
                .is_err()
        );
        assert_eq!(store.spotify_playlist_directory(&owner).unwrap(), accepted);
        assert_eq!(server.calls("/v1/me/playlists").len(), directory_reads);
        assert!(pool_from_store(&store).unwrap().get(OBSOLETE).is_some());
        store.close().unwrap();
        server.set("/v1/me/tracks", page(Vec::new(), 0, None));

        // A successful publication that retained prior taste after another
        // granted signal failed is not a complete reconciliation either.
        let mut store = SqliteStore::open(&file).unwrap();
        server.set("/v1/me/top/tracks", json!({ "not_a_page": true }));
        let mut session = server.session(&cx, &format!("{PLAYLIST_SCOPE} user-top-read"));
        let incomplete = sync_library_detailed(&mut session, &cx, &mut store)
            .await
            .unwrap();
        assert_eq!(incomplete.library.retired, 0);
        assert_eq!(incomplete.playlists.pending, 0);
        assert!(
            incomplete
                .notes
                .iter()
                .any(|note| note.contains("top tracks"))
        );
        assert_eq!(checkpoint(&store, &owner)["reconcile"], true);
        assert!(pool_from_store(&store).unwrap().get(OBSOLETE).is_some());
        store.close().unwrap();

        let mut store = SqliteStore::open(&file).unwrap();
        let directory_reads = server.calls("/v1/me/playlists").len();
        let reconciled = full(&server, &cx, &mut store).await;
        assert_eq!(server.calls("/v1/me/playlists").len() - directory_reads, 2);
        assert_eq!(reconciled.playlists.pending, 0);
        assert_eq!(reconciled.library.retired, 1);
        assert_eq!(pool_from_store(&store).unwrap().len(), OWNED);
        assert!(pool_from_store(&store).unwrap().get(OBSOLETE).is_none());
        assert_eq!(checkpoint(&store, &owner)["reconcile"], false);
        store.close().unwrap();
    });
}
