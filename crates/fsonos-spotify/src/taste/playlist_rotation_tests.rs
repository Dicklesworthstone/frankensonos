//! Bounded playlist intake through real HTTP, including durable continuation.

use super::*;
use crate::cache::{LibrarySyncDetailed, sync_library_detailed};
use fsonos_core::store::{CachedPlaylist, SqliteStore};

const OWNER: &str = "test-owner";

fn id(n: usize) -> String {
    format!("Rotation{n:03}")
}

fn uri(n: usize) -> String {
    format!("spotify:playlist:{}", id(n))
}

fn route(n: usize) -> String {
    format!("/v1/playlists/{}/items", id(n))
}

fn directory(server: &Server, ids: &[usize], owner: &str) {
    for (page_index, ids_on_page) in ids.chunks(50).enumerate() {
        let offset = page_index * 50;
        let after = offset + ids_on_page.len();
        let next = (after < ids.len()).then(|| format!("{API}/me/playlists?offset={after}"));
        let route = if offset == 0 {
            "/v1/me/playlists".into()
        } else {
            format!("/v1/me/playlists?offset={offset}")
        };
        let items: Vec<_> = ids_on_page
            .iter()
            .map(|&n| {
                json!({ "id": id(n), "name": format!("Playlist {n}"),
                "snapshot_id": "test-version-1",
                "owner": { "id": owner } })
            })
            .collect();
        server.set(
            &route,
            json!({ "items": items, "offset": offset,
            "total": ids.len(), "next": next }),
        );
    }
    for &n in ids {
        server.set(&route(n), page(vec![json!({ "item": track(n) })], 0, None));
    }
}

async fn sync(server: &Server, cx: &Cx, store: &mut dyn Store) -> LibrarySyncDetailed {
    // New sessions model successive daemon processes; scan progress is kept
    // by the store, never by the HTTP client or an in-memory iterator.
    let mut session = server.session(cx, PLAYLIST_SCOPE);
    sync_library_detailed(&mut session, cx, store)
        .await
        .unwrap()
}

fn observed(store: &dyn Store, n: usize) -> CachedPlaylist {
    store.playlist_contents(&uri(n)).unwrap().unwrap()
}

fn attempted(server: &Server, since: usize) -> Vec<String> {
    server.item_calls()[since..]
        .iter()
        .map(|call| {
            path(call)
                .strip_prefix("/v1/playlists/")
                .unwrap()
                .strip_suffix("/items")
                .unwrap()
                .to_owned()
        })
        .collect()
}

fn expected(ids: impl Iterator<Item = usize>) -> Vec<String> {
    ids.map(id).collect()
}

#[test]
fn windows_survive_restarts_reordering_and_a_removed_cursor_without_losing_music() {
    let server = Server::new();
    let mut ids: Vec<_> = (10..70).collect();
    ids.push(10); // A repeated directory entry must not use a second read.
    directory(&server, &ids, OWNER);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let path = server.data.join("playlist-rotation.db");
        let mut store = SqliteStore::open(&path).unwrap();
        let first = sync(&server, &cx, &mut store).await;
        assert_eq!(attempted(&server, 0), expected(10..35));
        assert!(
            first
                .notes
                .iter()
                .any(|note| note.contains("playlist budget"))
        );
        assert_eq!(pool_from_store(&store).unwrap().len(), 25);
        let previous = observed(&store, 10);
        assert!(store.playlist_contents(&uri(35)).unwrap().is_none());
        store.close().unwrap();

        // The cursor's playlist is removed, one earlier ID is added, and
        // the server reverses its order. Resume at the next surviving ID.
        let mut ids: Vec<_> = (10..70).filter(|&n| n != 34).chain([5]).rev().collect();
        ids.push(5);
        directory(&server, &ids, OWNER);
        let mut store = SqliteStore::open(&path).unwrap();
        sync(&server, &cx, &mut store).await;
        assert_eq!(attempted(&server, 25), expected(35..60));
        assert_eq!(
            observed(&store, 10),
            previous,
            "unvisited cache is unchanged"
        );
        assert_eq!(pool_from_store(&store).unwrap().len(), 50);
        assert_eq!(store.playlist_scan_cursor(OWNER).unwrap(), Some(id(59)));
        store.close().unwrap();

        let mut store = SqliteStore::open(&path).unwrap();
        sync(&server, &cx, &mut store).await;
        assert_eq!(
            attempted(&server, 50),
            expected((60..70).chain([5]).chain(10..24))
        );
        let pool = pool_from_store(&store).unwrap();
        for n in ids {
            assert!(observed(&store, n).complete);
            assert!(pool.get(&format!("spotify:track:TestTrack{n}")).is_some());
        }
        // Each incomplete taste window retains earlier candidates, including
        // the removed list's music; partial evidence cannot retire it.
        assert_eq!(pool.len(), 61);
        assert_eq!(server.item_calls().len(), 75);
        assert_eq!(server.calls("/v1/playlists/").len(), 150);
        store.close().unwrap();
    });
}

#[test]
fn a_rate_limit_advances_only_the_attempted_list_and_bad_lists_keep_last_good_contents() {
    let server = Server::new();
    directory(&server, &(0..60).collect::<Vec<_>>(), OWNER);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        for _ in 0..3 {
            sync(&server, &cx, &mut store).await;
        }
        assert_eq!(store.playlist_scan_cursor(OWNER).unwrap(), Some(id(14)));
        let old_limited = observed(&store, 15);
        let old_malformed = observed(&store, 16);
        server.state.lock().unwrap().refused.insert(route(15), 429);
        server.set(&route(16), json!({ "not_a_page": true }));

        let limited = sync(&server, &cx, &mut store).await;
        assert_eq!(
            attempted(&server, 75),
            [id(15)],
            "a halt reads no later lists"
        );
        assert_eq!(store.playlist_scan_cursor(OWNER).unwrap(), Some(id(15)));
        assert!(limited.notes.iter().any(|note| note.contains("429")));
        assert_eq!(observed(&store, 15), old_limited);
        assert_eq!(pool_from_store(&store).unwrap().len(), 60);

        // The bad list is still bad, but the following sync starts after it.
        // A malformed next list consumes one turn without stopping the rest.
        let resumed = sync(&server, &cx, &mut store).await;
        assert_eq!(attempted(&server, 76), expected(16..41));
        assert_eq!(store.playlist_scan_cursor(OWNER).unwrap(), Some(id(40)));
        assert!(
            resumed
                .notes
                .iter()
                .any(|note| note.contains("playlist items"))
        );
        assert_eq!(observed(&store, 15), old_limited);
        assert_eq!(observed(&store, 16), old_malformed);
        assert_eq!(pool_from_store(&store).unwrap().len(), 60);
    });
}

#[test]
fn switching_owners_does_not_borrow_or_reset_another_owners_scan() {
    let server = Server::new();
    let ids: Vec<_> = (0..30).collect();
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        for (owner, start, end) in [(OWNER, 0, 24), ("second-owner", 25, 24), (OWNER, 50, 19)] {
            directory(&server, &ids, owner);
            server.set("/v1/me", json!({ "id": owner }));
            let mut session = server.session(&cx, PLAYLIST_SCOPE);
            let (_, read) = read_taste(&mut session, &cx, &mut store, &[], 1).await;
            assert_eq!(read.playlists, 25);
            let actual = attempted(&server, start);
            let wanted = if start == 50 {
                expected((25..30).chain(0..20))
            } else {
                expected(0..25)
            };
            assert_eq!(actual, wanted);
            assert_eq!(store.playlist_scan_cursor(owner).unwrap(), Some(id(end)));
        }
        assert_eq!(
            store.playlist_scan_cursor("second-owner").unwrap(),
            Some(id(24))
        );
    });
}
