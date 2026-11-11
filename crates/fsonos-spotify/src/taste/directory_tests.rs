//! Directory reads beyond one budget, through real HTTP and reopened SQLite.

use super::*;
use crate::cache::{LibrarySyncDetailed, continue_playlists, sync_library_detailed};
use crate::client::PAGE_LIMIT;
use fsonos_core::store::{SpotifyLibraryOwner, SqliteStore};

const OWNER: &str = "test-owner";
const PREVIOUS: &str = "spotify:playlist:PreviousDirectory";
const WINDOW: usize = MAX_PAGES * PAGE_LIMIT as usize;

#[path = "directory_reconciliation_tests.rs"]
mod reconciliation;

fn id(n: usize) -> String {
    format!("Directory{n:05}")
}

fn uri(n: usize) -> String {
    format!("spotify:playlist:{}", id(n))
}

fn populate(server: &Server, total: usize, base: usize) {
    for offset in (0..total).step_by(PAGE_LIMIT as usize) {
        let end = (offset + PAGE_LIMIT as usize).min(total);
        let items = (offset..end)
            .map(|n| {
                // Duplicate listings still occupy raw source positions. They
                // must not shift the checkpoint or spend two content windows.
                let n = if n == 1 { 0 } else { n };
                json!({
                    "id": id(base + n), "name": format!("Directory list {}", base + n),
                    "owner": { "id": if n + 1 == total { OWNER } else { "another-owner" } },
                    "snapshot_id": "test-version-1"
                })
            })
            .collect::<Vec<_>>();
        server.set(
            &format!("/v1/me/playlists?offset={offset}"),
            json!({ "items": items, "offset": offset, "total": total,
                "next": (end < total).then(|| format!("{API}/me/playlists?offset={end}")) }),
        );
    }
    server.set(
        &format!("/v1/playlists/{}/items", id(base + total - 1)),
        page(vec![json!({ "item": track(base + total - 1) })], 0, None),
    );
}

async fn full(server: &Server, cx: &Cx, store: &mut dyn Store) -> LibrarySyncDetailed {
    let mut session = server.session(cx, PLAYLIST_SCOPE);
    sync_library_detailed(&mut session, cx, store)
        .await
        .unwrap()
}

async fn pending(server: &Server, cx: &Cx, store: &mut dyn Store) -> LibrarySyncDetailed {
    let mut session = server.session(cx, PLAYLIST_SCOPE);
    continue_playlists(&mut session, cx, store, OWNER)
        .await
        .unwrap()
}

fn seed(store: &mut dyn Store) -> SpotifyLibraryOwner {
    let owner = store.adopt_spotify_library_owner(OWNER).unwrap().0;
    store
        .save_playlists(&[(PREVIOUS.into(), "Last accepted directory".into())])
        .unwrap();
    store
        .save_playlist_progress(OWNER, PREVIOUS, "last accepted proof")
        .unwrap();
    owner
}

fn checkpoint(store: &dyn Store, owner: &SpotifyLibraryOwner) -> Value {
    serde_json::from_str(&store.spotify_playlist_directory(owner).unwrap().unwrap()).unwrap()
}

#[test]
fn a_directory_larger_than_two_budgets_reaches_its_last_playlist_across_restarts() {
    let server = Server::new();
    let total = WINDOW * 2 + 51;
    populate(&server, total, 0);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let file = server.data.join("large-directory.db");
        let mut store = SqliteStore::open(&file).unwrap();
        let owner = seed(&mut store);
        let original = store.playlists().unwrap();
        let first = full(&server, &cx, &mut store).await;
        assert_eq!(
            (first.playlists.pending, first.playlists.advanced),
            (1, WINDOW as u32)
        );
        assert_eq!(server.calls("/v1/me/playlists").len(), MAX_PAGES);
        assert_eq!(checkpoint(&store, &owner)["examined"], WINDOW);
        assert_eq!(store.playlists().unwrap(), original);
        assert!(store.playlist_progress(OWNER, PREVIOUS).unwrap().is_some());
        assert!(
            server.item_calls().is_empty(),
            "the owned playlist is beyond the first budget"
        );
        store.close().unwrap();

        let before = server.calls("/v1/").len();
        for (examined, remaining) in [(WINDOW * 2 - 100, 1), (total, 0)] {
            let mut store = SqliteStore::open(&file).unwrap();
            let calls = server.calls("/v1/me/playlists").len();
            let continued = pending(&server, &cx, &mut store).await;
            assert_eq!(continued.playlists.pending, remaining);
            assert!(continued.playlists.advanced > 0);
            assert!(server.calls("/v1/me/playlists").len() - calls <= MAX_PAGES);
            assert_eq!(checkpoint(&store, &owner)["examined"], examined);
            if remaining > 0 {
                assert_eq!(store.playlists().unwrap(), original);
                assert!(store.playlist_progress(OWNER, PREVIOUS).unwrap().is_some());
            }
            store.close().unwrap();
        }
        let mut store = SqliteStore::open(&file).unwrap();
        let listed = store.playlists().unwrap();
        assert_eq!(
            listed.len(),
            total - 1,
            "raw duplicate positions are deduplicated only at publication"
        );
        assert_eq!(listed.last().unwrap().0, uri(total - 1));
        assert!(store.playlist_progress(OWNER, PREVIOUS).unwrap().is_none());
        assert!(
            store
                .playlist_contents(&uri(total - 1))
                .unwrap()
                .unwrap()
                .complete
        );
        assert!(
            pool_from_store(&store)
                .unwrap()
                .get(&format!("spotify:track:TestTrack{}", total - 1))
                .is_some()
        );
        let calls = server.calls("/v1/me/playlists").len();
        let item_reads = server.item_calls().len();
        let done = pending(&server, &cx, &mut store).await;
        assert_eq!((done.playlists.pending, done.playlists.advanced), (0, 0));
        assert_eq!(
            server.calls("/v1/me/playlists").len() - calls,
            2,
            "complete directories are checked, not restarted every continuation"
        );
        assert_eq!(server.item_calls().len(), item_reads);
        assert!(server.calls("/v1/")[before..].iter().all(|call| {
            matches!(path(call), "/v1/me" | "/v1/me/playlists")
                || path(call).starts_with("/v1/playlists/")
        }));
        store.close().unwrap();
    });
}

#[test]
fn exact_directory_budget_completion_does_not_schedule_another_scan() {
    let server = Server::new();
    populate(&server, WINDOW, 0);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        let result = full(&server, &cx, &mut store).await;
        assert_eq!(result.playlists.pending, 0);
        assert_eq!(store.playlists().unwrap().len(), WINDOW - 1);
        assert_eq!(server.calls("/v1/me/playlists").len(), MAX_PAGES);
        assert_eq!(server.item_calls().len(), 1);
    });
}

#[test]
fn failed_or_invalid_resume_pages_preserve_the_last_checkpoint_and_published_directory() {
    let server = Server::new();
    let total = WINDOW + 51;
    populate(&server, total, 0);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        let owner = seed(&mut store);
        full(&server, &cx, &mut store).await;
        let mut before = checkpoint(&store, &owner);
        assert_eq!(before["reconcile"], false);
        // A background attempt records the full reconciliation it will owe,
        // even when no additional directory page can be accepted this time.
        before["reconcile"] = true.into();
        let mut previous_publication = before["publication"].take();
        let listed = store.playlists().unwrap();
        let route = format!("/v1/me/playlists?offset={WINDOW}");
        let good = server.state.lock().unwrap().pages[&route].clone();
        let mut bad_pages = vec![json!({ "items": "bad page" })];
        for (field, value) in [
            ("offset", json!(WINDOW - 1)),
            ("total", json!(total + 1)),
            (
                "next",
                json!(format!("{API}/me/playlists?offset={}", WINDOW + 49)),
            ),
            (
                "next",
                json!(format!("{API}/me/tracks?offset={}", WINDOW + 50)),
            ),
            (
                "next",
                json!(format!(
                    "https://example.invalid/v1/me/playlists?offset={}",
                    WINDOW + 50
                )),
            ),
            ("next", Value::Null),
        ] {
            let mut bad = good.clone();
            bad[field] = value;
            bad_pages.push(bad);
        }
        for bad in bad_pages {
            server.set(&route, bad);
            let failed = pending(&server, &cx, &mut store).await;
            assert_eq!(
                (failed.playlists.pending, failed.playlists.advanced),
                (1, 0)
            );
            let mut after = checkpoint(&store, &owner);
            let publication = after["publication"].take();
            assert!(publication.as_str().is_some_and(|value| !value.is_empty()));
            assert_ne!(
                publication, previous_publication,
                "accepting retained playlist observations fences older full refreshes"
            );
            previous_publication = publication;
            assert_eq!(after, before, "failed intake preserves the accepted prefix");
            assert_eq!(store.playlists().unwrap(), listed);
            assert!(store.playlist_progress(OWNER, PREVIOUS).unwrap().is_some());
            assert!(!failed.notes.is_empty());
        }
        server.set(&route, good);
        let completed = pending(&server, &cx, &mut store).await;
        assert_eq!(completed.playlists.pending, 0);
        assert_eq!(store.playlists().unwrap().last().unwrap().0, uri(total - 1));
        assert!(store.playlist_progress(OWNER, PREVIOUS).unwrap().is_none());
        assert_eq!(
            server.calls("/v1/me/tracks").len(),
            1,
            "a changed next link was never followed"
        );
    });
}

#[test]
fn changed_head_or_resume_boundary_restarts_without_publishing_a_mixed_directory() {
    for boundary_only in [false, true] {
        let server = Server::new();
        let total = WINDOW + 1;
        populate(&server, total, 0);
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let mut store = MemStore::default();
            let owner = seed(&mut store);
            full(&server, &cx, &mut store).await;
            let listed = store.playlists().unwrap();
            let replacement = if boundary_only {
                let route = format!("/v1/me/playlists?offset={}", WINDOW - 50);
                let mut changed = server.state.lock().unwrap().pages[&route].clone();
                changed["items"][0]["id"] = json!("ReplacementBoundary");
                server.set(&route, changed);
                "spotify:playlist:ReplacementBoundary".to_owned()
            } else {
                populate(&server, total, 5_000);
                uri(5_000)
            };
            let restarted = pending(&server, &cx, &mut store).await;
            assert_eq!(restarted.playlists.pending, 1);
            assert_eq!(store.playlists().unwrap(), listed);
            assert!(store.playlist_progress(OWNER, PREVIOUS).unwrap().is_some());
            assert_eq!(
                checkpoint(&store, &owner)["examined"],
                if boundary_only { WINDOW - 50 } else { WINDOW }
            );
            assert_eq!(pending(&server, &cx, &mut store).await.playlists.pending, 0);
            let listed = store.playlists().unwrap();
            assert!(listed.iter().any(|(uri, _)| uri == &replacement));
            if !boundary_only {
                assert!(!listed.iter().any(|(candidate, _)| candidate == &uri(0)));
            }
        });
    }
}

#[test]
fn an_account_switch_during_directory_read_cannot_checkpoint_publish_or_prune_the_new_adoption() {
    for returning in [false, true] {
        let server = Server::new();
        populate(&server, WINDOW + 1, 0);
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let file = server.data.join("directory-owner-switch.db");
            let mut store = SqliteStore::open(&file).unwrap();
            seed(&mut store);
            full(&server, &cx, &mut store).await;
            let competing_file = file.clone();
            server.state.lock().unwrap().before_response = Some((
                "/v1/me/playlists".into(),
                Box::new(move || {
                    let mut competing = SqliteStore::open(&competing_file).unwrap();
                    let (second, _) = competing
                        .adopt_spotify_library_owner("second-owner")
                        .unwrap();
                    let owner = if returning {
                        competing.adopt_spotify_library_owner(OWNER).unwrap().0
                    } else {
                        second
                    };
                    assert!(
                        competing
                            .save_spotify_playlist_directory(
                                &owner,
                                None,
                                "new adoption checkpoint",
                                None
                            )
                            .unwrap()
                    );
                    competing
                        .save_playlist_progress(&owner.owner_id, PREVIOUS, "new adoption proof")
                        .unwrap();
                    assert!(
                        competing
                            .publish_spotify_library(
                                &owner,
                                &[],
                                Some(&[(PREVIOUS.into(), "New adoption's directory".into())]),
                                &[],
                                None
                            )
                            .unwrap()
                    );
                    competing.close().unwrap();
                }),
            ));
            let reads = server.item_calls().len();
            let failed = pending(&server, &cx, &mut store).await;
            assert!(failed.playlists.requires_refresh);
            assert_eq!(server.item_calls().len(), reads);
            let current = store.spotify_library_owner().unwrap().unwrap();
            assert_eq!(
                store
                    .spotify_playlist_directory(&current)
                    .unwrap()
                    .as_deref(),
                Some("new adoption checkpoint")
            );
            assert_eq!(
                store
                    .playlist_progress(&current.owner_id, PREVIOUS)
                    .unwrap()
                    .as_deref(),
                Some("new adoption proof")
            );
            assert_eq!(
                store.playlists().unwrap(),
                [(PREVIOUS.into(), "New adoption's directory".into())]
            );
            store.close().unwrap();
        });
    }
}

#[test]
fn invalid_directory_checkpoints_restart_without_accepting_their_claimed_coverage() {
    for field in ["version", "generation", "first_end", "examined"] {
        let server = Server::new();
        populate(&server, WINDOW + 1, 0);
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let mut store = MemStore::default();
            let owner = seed(&mut store);
            full(&server, &cx, &mut store).await;
            let before = store.spotify_playlist_directory(&owner).unwrap().unwrap();
            let mut invalid: Value = serde_json::from_str(&before).unwrap();
            invalid[field] = json!(9_999_999);
            assert!(
                store
                    .save_spotify_playlist_directory(
                        &owner,
                        Some(&before),
                        &invalid.to_string(),
                        None
                    )
                    .unwrap()
            );
            assert_eq!(pending(&server, &cx, &mut store).await.playlists.pending, 1);
            assert_eq!(checkpoint(&store, &owner)["examined"], WINDOW);
            assert_eq!(checkpoint(&store, &owner)["generation"], owner.generation);
            assert!(store.playlist_progress(OWNER, PREVIOUS).unwrap().is_some());
            assert_eq!(pending(&server, &cx, &mut store).await.playlists.pending, 0);
        });
    }
}

#[test]
fn cached_middle_directory_rows_revalidate_changed_unfinished_playlists() {
    let server = Server::new();
    populate(&server, 151, 0);
    let middle = 75;
    let mid_route = "/v1/me/playlists?offset=50";
    let mut page = server.state.lock().unwrap().pages[mid_route].clone();
    page["items"][25]["owner"] = json!({ "id": OWNER });
    page["items"][25]["snapshot_id"] = json!("original");
    server.set(mid_route, page);
    let tail_route = "/v1/me/playlists?offset=150";
    let mut tail = server.state.lock().unwrap().pages[tail_route].clone();
    tail["items"][0]["owner"] = json!({ "id": "another-owner" });
    server.set(tail_route, tail);
    let contents = |version: &str, first: usize| {
        server.set(
            &format!("/v1/playlists/{}", id(middle)),
            json!({ "id": id(middle), "snapshot_id": version }),
        );
        for offset in (0..351).step_by(50) {
            let end = (offset + 50).min(351);
            server.set(
                &format!("/v1/playlists/{}/items?offset={offset}", id(middle)),
                json!({ "items": (offset..end).map(|n| json!({ "item": track(first + n) })).collect::<Vec<_>>(),
                    "offset": offset, "total": 351,
                    "next": (end < 351).then(|| format!("{API}/playlists/{}/items?offset={end}", id(middle))) }),
            );
        }
    };
    contents("original", 0);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        assert_eq!(full(&server, &cx, &mut store).await.playlists.pending, 1);
        contents("edited", 10_000);
        let mut changed = server.state.lock().unwrap().pages[mid_route].clone();
        changed["items"][25]["snapshot_id"] = json!("edited");
        server.set(mid_route, changed);
        let calls = server.calls("/v1/playlists/").len();
        let restarted = pending(&server, &cx, &mut store).await;
        assert_eq!(restarted.playlists.pending, 1);
        assert!(server.calls("/v1/playlists/").len() - calls <= 4);
        let progress: Value = serde_json::from_str(
            &store
                .playlist_progress(OWNER, &uri(middle))
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(progress["snapshot_id"], "edited");
        assert_eq!(
            progress["examined"], 100,
            "the extra snapshot preflight stays inside the four-request window"
        );
        for remaining in [1, 1, 0] {
            assert_eq!(
                pending(&server, &cx, &mut store).await.playlists.pending,
                remaining
            );
        }
        let observed = store.playlist_contents(&uri(middle)).unwrap().unwrap();
        assert!(observed.complete);
        assert_eq!(observed.tracks.len(), 351);
        assert_eq!(
            observed.tracks[0].source_uri,
            "spotify:track:TestTrack10000"
        );
        assert_eq!(
            observed.tracks.last().unwrap().source_uri,
            "spotify:track:TestTrack10350"
        );
    });
}

#[test]
fn a_terminal_directory_limit_waits_for_full_refresh_instead_of_repeating_background_reads() {
    let server = Server::new();
    populate(&server, WINDOW + 1, 0);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        let owner = seed(&mut store);
        full(&server, &cx, &mut store).await;
        let accepted = store.spotify_playlist_directory(&owner).unwrap().unwrap();
        let mut marker: Value = serde_json::from_str(&accepted).unwrap();
        for name in ["examined", "first_end", "last_start"] {
            marker[name] = json!(0);
        }
        marker["items"] = json!([]);
        marker["complete"] = json!(false);
        marker["limited"] = json!(true);
        assert!(
            store
                .save_spotify_playlist_directory(&owner, Some(&accepted), &marker.to_string(), None)
                .unwrap()
        );
        let listed = store.playlists().unwrap();
        let calls = server.calls("/v1/me/playlists").len();
        for _ in 0..2 {
            let limited = pending(&server, &cx, &mut store).await;
            assert_eq!(
                (
                    limited.playlists.pending,
                    limited.playlists.limited,
                    limited.playlists.advanced
                ),
                (0, 1, 0)
            );
            assert_eq!(server.calls("/v1/me/playlists").len(), calls);
            assert_eq!(store.playlists().unwrap(), listed);
            assert!(store.playlist_progress(OWNER, PREVIOUS).unwrap().is_some());
        }
        let retry = full(&server, &cx, &mut store).await;
        assert_eq!((retry.playlists.pending, retry.playlists.limited), (1, 0));
        assert_eq!(pending(&server, &cx, &mut store).await.playlists.pending, 0);
    });
}
