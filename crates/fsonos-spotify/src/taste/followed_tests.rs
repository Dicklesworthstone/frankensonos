//! Followed membership through the production client, loopback HTTP and SQLite.

use super::*;
use crate::cache::{LibrarySyncDetailed, continue_followed_artists, sync_library_detailed};
use crate::client::PAGE_LIMIT;
use crate::taste::{ARTIST_PM, FollowedArtistPhase};
use fsonos_core::store::{SpotifyLibraryOwner, SqliteStore};

const OWNER: &str = "test-owner";
const SCOPE: &str = "user-follow-read";
const WINDOW: usize = MAX_PAGES * PAGE_LIMIT as usize;
const OBSOLETE: &str = "spotify:track:OldFollowedTaste";
const LIKED: &str = "spotify:track:TestTrack99999";

fn id(n: usize) -> String {
    format!("FollowedArtist{n:06}")
}

fn name(n: usize) -> String {
    format!("Test followed performer {n}")
}

fn route(start: usize) -> String {
    if start == 0 {
        "/v1/me/following".into()
    } else {
        format!(
            "/v1/me/following?type=artist&limit=50&after={}",
            id(start - 1)
        )
    }
}

fn populate(server: &Server, total: usize) {
    for start in (0..total.max(1)).step_by(PAGE_LIMIT as usize) {
        let end = (start + PAGE_LIMIT as usize).min(total);
        let items = (start..end)
            .map(|n| json!({ "id": id(n), "name": name(n), "genres": [] }))
            .collect::<Vec<_>>();
        let next = (end < total).then(|| {
            format!(
                "{API}/me/following?type=artist&limit=50&after={}",
                id(end - 1)
            )
        });
        server.set(
            &route(start),
            json!({ "artists": {
                "items": items, "total": total, "next": next,
                "cursors": { "after": (end > 0).then(|| id(end - 1)) }, "limit": 50
            }}),
        );
    }
    // A complete empty sample is valid. Avoid catalog fallback traffic while
    // exercising the actual full-sync taste and publication paths.
    for n in 0..total {
        server.set(
            &format!("/v1/artists/{}/top-tracks", id(n)),
            json!({ "tracks": [] }),
        );
    }
    let last = total.saturating_sub(1);
    let mut liked = track(99_999);
    liked["artists"] = json!([{ "id": id(last), "name": name(last) }]);
    server.set(
        "/v1/me/tracks",
        page(vec![json!({ "track": liked })], 0, None),
    );
    server.set(
        &format!("/v1/artists/{}", id(last)),
        json!({
            "id": id(last), "name": name(last), "genres": ["folk"]
        }),
    );
}

fn seed(store: &mut dyn Store) -> SpotifyLibraryOwner {
    let owner = store.adopt_spotify_library_owner(OWNER).unwrap().0;
    apply_library_read(
        store,
        &[LibraryItem {
            source_uri: OBSOLETE.into(),
            title: "Previous taste song".into(),
            artists: vec!["Previous followed performer".into()],
            origin: Origin::Taste,
            ..LibraryItem::default()
        }],
    )
    .unwrap();
    owner
}

async fn full(server: &Server, cx: &Cx, store: &mut dyn Store) -> LibrarySyncDetailed {
    let mut session = server.session(cx, SCOPE);
    sync_library_detailed(&mut session, cx, store)
        .await
        .unwrap()
}

async fn pending(server: &Server, cx: &Cx, store: &mut dyn Store) -> LibrarySyncDetailed {
    let mut session = server.session(cx, SCOPE);
    continue_followed_artists(&mut session, cx, store, OWNER)
        .await
        .unwrap()
}

fn checkpoint(store: &dyn Store, owner: &SpotifyLibraryOwner) -> Value {
    serde_json::from_str(&store.spotify_followed_directory(owner).unwrap().unwrap()).unwrap()
}

#[test]
fn a_large_followed_directory_survives_restart_verifies_and_reconciles_once() {
    let server = Server::new();
    let total = WINDOW * 2 + 51;
    populate(&server, total);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let file = server.data.join("large-followed.db");
        let mut store = SqliteStore::open(&file).unwrap();
        let owner = seed(&mut store);
        let first = full(&server, &cx, &mut store).await;
        assert_eq!(first.followed_artists.artists, WINDOW);
        assert_eq!(first.followed_artists.pages, MAX_PAGES);
        assert!(first.followed_artists.pending);
        assert_eq!(first.followed_artists.phase, FollowedArtistPhase::Reading);
        assert_eq!(server.calls("/v1/me/following").len(), MAX_PAGES);
        assert!(pool_from_store(&store).unwrap().get(OBSOLETE).is_some());
        let public = store.library().unwrap();
        store.close().unwrap();

        let mut saw_verification = false;
        let mut ready = false;
        for _ in 0..8 {
            let mut store = SqliteStore::open(&file).unwrap();
            let calls = server.calls("/v1/").len();
            let pages = server.calls("/v1/me/following").len();
            let result = pending(&server, &cx, &mut store).await;
            assert_eq!(
                store.library().unwrap(),
                public,
                "background membership never publishes a copied library"
            );
            assert!(
                result.followed_artists.pending,
                "ready still owes a full publication"
            );
            assert!(server.calls("/v1/me/following").len() - pages <= MAX_PAGES);
            assert!(
                server.calls("/v1/")[calls..]
                    .iter()
                    .all(|call| { matches!(path(call), "/v1/me" | "/v1/me/following") })
            );
            saw_verification |= result.followed_artists.phase == FollowedArtistPhase::Verifying;
            ready = result.followed_artists.ready();
            if ready {
                assert_eq!(result.followed_artists.artists, total);
                assert_eq!(result.followed_artists.verified_pages, total.div_ceil(50));
                assert_eq!(checkpoint(&store, &owner)["applied"], false);
            }
            store.close().unwrap();
            if ready {
                break;
            }
        }
        assert!(
            ready && saw_verification,
            "the directory must eventually finish every retained page"
        );
        let mut store = SqliteStore::open(&file).unwrap();
        let pages = server.calls("/v1/me/following").len();
        let reconciled = full(&server, &cx, &mut store).await;
        assert!(!reconciled.followed_artists.pending);
        assert_eq!(
            reconciled.followed_artists.phase,
            FollowedArtistPhase::Ready
        );
        assert_eq!(reconciled.followed_artists.advanced, 0);
        assert_eq!(server.calls("/v1/me/following").len() - pages, 2);
        assert_eq!(checkpoint(&store, &owner)["applied"], true);
        assert!(pool_from_store(&store).unwrap().get(OBSOLETE).is_none());
        assert_eq!(
            store
                .library()
                .unwrap()
                .iter()
                .find(|entry| entry.track.source_uri == LIKED)
                .unwrap()
                .taste_weight,
            Some(ARTIST_PM)
        );

        let pages = server.calls("/v1/me/following").len();
        let refreshed = full(&server, &cx, &mut store).await;
        assert!(refreshed.followed_artists.pending);
        assert_eq!(
            refreshed.followed_artists.phase,
            FollowedArtistPhase::Reading
        );
        assert_eq!(server.calls("/v1/me/following").len() - pages, MAX_PAGES);
        store.close().unwrap();
    });
}

#[test]
fn an_exact_page_budget_finishes_without_a_background_scan() {
    let server = Server::new();
    populate(&server, WINDOW);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        let owner = seed(&mut store);
        let result = full(&server, &cx, &mut store).await;
        assert_eq!(result.followed_artists.artists, WINDOW);
        assert_eq!(result.followed_artists.phase, FollowedArtistPhase::Ready);
        assert!(!result.followed_artists.pending);
        assert_eq!(result.followed_artists.verified_pages, MAX_PAGES);
        assert_eq!(checkpoint(&store, &owner)["applied"], true);
        assert_eq!(server.calls("/v1/me/following").len(), MAX_PAGES);
        assert!(pool_from_store(&store).unwrap().get(OBSOLETE).is_none());
    });
}

#[test]
fn malformed_or_untrusted_next_pages_never_advance_private_or_public_membership() {
    let server = Server::new();
    populate(&server, WINDOW + 51);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        let owner = seed(&mut store);
        full(&server, &cx, &mut store).await;
        let original = checkpoint(&store, &owner);
        let public = store.library().unwrap();
        let next = route(WINDOW);
        let good = server.state.lock().unwrap().pages[&next].clone();
        let mut bad_pages = Vec::new();
        for field in ["items", "next"] {
            let mut bad = good.clone();
            bad["artists"].as_object_mut().unwrap().remove(field);
            bad_pages.push(bad);
        }
        for (field, value) in [
            ("next", Value::Null),
            (
                "next",
                json!(format!(
                    "{API}/me/following?type=user&after={}",
                    id(WINDOW + 49)
                )),
            ),
            (
                "next",
                json!(format!(
                    "https://example.invalid/v1/me/following?type=artist&after={}",
                    id(WINDOW + 49)
                )),
            ),
            (
                "next",
                json!(format!(
                    "{API}/me/following?type=artist&after={}",
                    id(WINDOW + 48)
                )),
            ),
            (
                "next",
                json!(format!(
                    "{API}/me/following?type=artist&after={}&after={}",
                    id(WINDOW + 49),
                    id(WINDOW + 49)
                )),
            ),
        ] {
            let mut bad = good.clone();
            bad["artists"][field] = value;
            bad_pages.push(bad);
        }
        let mut oversized = good.clone();
        let extra = oversized["artists"]["items"][0].clone();
        oversized["artists"]["items"]
            .as_array_mut()
            .unwrap()
            .push(extra);
        bad_pages.push(oversized);
        for bad in bad_pages {
            server.set(&next, bad);
            let result = pending(&server, &cx, &mut store).await;
            assert_eq!(result.followed_artists.artists, WINDOW);
            assert_eq!(result.followed_artists.pages, MAX_PAGES);
            assert_eq!(result.followed_artists.advanced, 0);
            assert!(result.followed_artists.pending);
            assert!(!result.notes.is_empty());
            assert_eq!(checkpoint(&store, &owner)["pages"], original["pages"]);
            assert_eq!(store.library().unwrap(), public);
        }
        server.set(&next, good);
        assert!(
            pending(&server, &cx, &mut store)
                .await
                .followed_artists
                .advanced
                > 0
        );
        assert!(
            server
                .calls("/v1/")
                .iter()
                .all(|call| !call.contains("type=user"))
        );
    });
}

#[test]
fn a_changed_middle_page_is_detected_before_old_taste_can_be_retired() {
    let server = Server::new();
    populate(&server, WINDOW + 1);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        seed(&mut store);
        full(&server, &cx, &mut store).await;
        server
            .state
            .lock()
            .unwrap()
            .pages
            .get_mut(&route(250))
            .unwrap()["artists"]["items"][0]["name"] = json!("Changed middle performer");
        let result = pending(&server, &cx, &mut store).await;
        assert!(result.followed_artists.pending);
        assert_eq!(result.followed_artists.phase, FollowedArtistPhase::Reading);
        assert_eq!(result.followed_artists.pages, 0);
        assert_eq!(result.followed_artists.advanced, 0);
        assert!(
            result
                .notes
                .iter()
                .any(|note| note.contains("retained followed artist page changed"))
        );
        assert!(pool_from_store(&store).unwrap().get(OBSOLETE).is_some());
        let mut ready = false;
        for _ in 0..5 {
            ready = pending(&server, &cx, &mut store)
                .await
                .followed_artists
                .ready();
            if ready {
                break;
            }
        }
        assert!(ready);
        assert!(
            !full(&server, &cx, &mut store)
                .await
                .followed_artists
                .pending
        );
        assert!(pool_from_store(&store).unwrap().get(OBSOLETE).is_none());
    });
}

#[test]
fn a_changed_total_or_repeated_earlier_artist_restarts_instead_of_retrying_forever() {
    for total_changed in [false, true] {
        let server = Server::new();
        populate(&server, WINDOW + 51);
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let mut store = MemStore::default();
            let owner = seed(&mut store);
            full(&server, &cx, &mut store).await;
            let original = checkpoint(&store, &owner);
            let next = route(WINDOW);
            let good = server.state.lock().unwrap().pages[&next].clone();
            let mut changed = good.clone();
            if total_changed {
                changed["artists"]["total"] = json!(WINDOW + 52);
            } else {
                changed["artists"]["items"][0]["id"] = json!(id(0));
            }
            server.set(&next, changed);
            let restarted = pending(&server, &cx, &mut store).await;
            assert!(restarted.followed_artists.pending);
            assert_eq!(restarted.followed_artists.pages, 0);
            assert_eq!(restarted.followed_artists.advanced, 0);
            assert_ne!(checkpoint(&store, &owner)["nonce"], original["nonce"]);
            assert!(pool_from_store(&store).unwrap().get(OBSOLETE).is_some());
            server.set(&next, good);
            let mut ready = false;
            for _ in 0..5 {
                ready = pending(&server, &cx, &mut store)
                    .await
                    .followed_artists
                    .ready();
                if ready {
                    break;
                }
            }
            assert!(ready, "a fresh bounded scan must escape the invalid prefix");
        });
    }
}

#[test]
fn rate_limits_preserve_the_cursor_and_block_later_reads_in_the_same_session() {
    let server = Server::new();
    populate(&server, WINDOW + 1);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        let owner = seed(&mut store);
        full(&server, &cx, &mut store).await;
        let before = checkpoint(&store, &owner)["pages"].clone();
        server
            .state
            .lock()
            .unwrap()
            .refused
            .insert("/v1/me/following".into(), 429);
        let mut session = server.session(&cx, SCOPE);
        let result = continue_followed_artists(&mut session, &cx, &mut store, OWNER)
            .await
            .unwrap();
        assert!(result.followed_artists.pending);
        assert!(result.followed_artists.retry_after_secs > 3_000);
        assert_eq!(checkpoint(&store, &owner)["pages"], before);
        let calls = server.calls("/v1/").len();
        assert!(session.read_owner(&cx).await.is_err());
        assert_eq!(server.calls("/v1/").len(), calls);
    });
}

#[test]
fn an_account_switch_including_away_and_back_fences_the_reader() {
    for returning in [false, true] {
        let server = Server::new();
        populate(&server, WINDOW + 1);
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let file = server.data.join("followed-account-switch.db");
            let mut store = SqliteStore::open(&file).unwrap();
            let owner = seed(&mut store);
            full(&server, &cx, &mut store).await;
            let competing_file = file.clone();
            server.state.lock().unwrap().before_response = Some((
                "/v1/me/following".into(),
                Box::new(move || {
                    let mut other = SqliteStore::open(&competing_file).unwrap();
                    let (mut owner, _) = other.adopt_spotify_library_owner("second-owner").unwrap();
                    if returning {
                        owner = other.adopt_spotify_library_owner(OWNER).unwrap().0;
                    }
                    assert!(
                        other
                            .save_spotify_followed_directory(
                                &owner,
                                None,
                                "new adoption checkpoint"
                            )
                            .unwrap()
                    );
                    apply_library_read(
                        &mut other,
                        &[LibraryItem {
                            source_uri: "spotify:track:NewAdoption".into(),
                            title: "New adoption song".into(),
                            artists: vec!["New adoption performer".into()],
                            ..LibraryItem::default()
                        }],
                    )
                    .unwrap();
                    other.close().unwrap();
                }),
            ));
            let result = pending(&server, &cx, &mut store).await;
            assert!(result.followed_artists.requires_refresh);
            assert!(store.spotify_followed_directory(&owner).unwrap().is_none());
            let current = store.spotify_library_owner().unwrap().unwrap();
            assert_eq!(
                store
                    .spotify_followed_directory(&current)
                    .unwrap()
                    .as_deref(),
                Some("new adoption checkpoint")
            );
            assert!(
                pool_from_store(&store)
                    .unwrap()
                    .get("spotify:track:NewAdoption")
                    .is_some()
            );
            store.close().unwrap();
        });
    }
}

#[test]
fn a_competing_same_owner_claim_and_publication_are_never_overwritten() {
    let server = Server::new();
    populate(&server, 1);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let file = server.data.join("followed-competing-reader.db");
        let mut store = SqliteStore::open(&file).unwrap();
        let owner = seed(&mut store);
        let competing_file = file.clone();
        server.state.lock().unwrap().before_response = Some((
            "/v1/me/following".into(),
            Box::new(move || {
                let mut other = SqliteStore::open(&competing_file).unwrap();
                let owner = other.spotify_library_owner().unwrap().unwrap();
                let previous = other.spotify_followed_directory(&owner).unwrap().unwrap();
                let mut progress: Value = serde_json::from_str(&previous).unwrap();
                progress["nonce"] = json!("competing-reader-claim");
                assert!(
                    other
                        .save_spotify_followed_directory(
                            &owner,
                            Some(&previous),
                            &progress.to_string()
                        )
                        .unwrap()
                );
                apply_library_read(
                    &mut other,
                    &[LibraryItem {
                        source_uri: "spotify:track:ConcurrentSameOwner".into(),
                        title: "New concurrent song".into(),
                        artists: vec!["Concurrent performer".into()],
                        ..LibraryItem::default()
                    }],
                )
                .unwrap();
                other.close().unwrap();
            }),
        ));
        let result = pending(&server, &cx, &mut store).await;
        assert!(result.followed_artists.pending);
        assert!(!result.followed_artists.requires_refresh);
        assert_eq!(
            checkpoint(&store, &owner)["nonce"],
            "competing-reader-claim"
        );
        assert!(
            result
                .notes
                .iter()
                .any(|note| note.contains("checkpoint changed"))
        );
        assert!(
            pool_from_store(&store)
                .unwrap()
                .get("spotify:track:ConcurrentSameOwner")
                .is_some()
        );
        assert!(pool_from_store(&store).unwrap().get(OBSOLETE).is_none());
        store.close().unwrap();
    });
}

#[test]
fn an_unchanged_ready_directory_gets_a_new_receipt_without_inventing_progress() {
    let server = Server::new();
    populate(&server, 1);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        let owner = seed(&mut store);
        let mut session = server.session(&cx, SCOPE);
        let (old_taste, first) =
            crate::taste::read_pending_followed(&mut session, &cx, &mut store, &owner, 100).await;
        assert!(first.followed_continuation.ready());
        assert_eq!(first.followed_continuation.advanced, 1);
        let before = checkpoint(&store, &owner);
        let (new_taste, again) =
            crate::taste::read_pending_followed(&mut session, &cx, &mut store, &owner, 100).await;
        assert!(again.followed_continuation.ready());
        assert_eq!(again.followed_continuation.advanced, 0);
        let after = checkpoint(&store, &owner);
        assert_ne!(before["nonce"], after["nonce"]);
        assert_eq!(before["pages"], after["pages"]);
        assert!(
            store
                .publish_spotify_library(&owner, &[], None, &[], None)
                .unwrap()
        );
        assert!(
            !crate::taste::finish_followed_reconciliation(
                &mut store,
                &owner,
                old_taste.followed.as_ref()
            )
            .unwrap()
        );
        assert!(!checkpoint(&store, &owner)["applied"].as_bool().unwrap());
        assert!(
            crate::taste::finish_followed_reconciliation(
                &mut store,
                &owner,
                new_taste.followed.as_ref()
            )
            .unwrap()
        );
        assert!(checkpoint(&store, &owner)["applied"].as_bool().unwrap());
    });
}

#[test]
fn a_missing_follow_scope_is_unobserved_and_makes_no_http_requests() {
    let server = Server::new();
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        seed(&mut store);
        let mut session = server.session(&cx, "user-library-read");
        let result = continue_followed_artists(&mut session, &cx, &mut store, OWNER)
            .await
            .unwrap();
        assert!(result.followed_artists.owner.is_none());
        assert!(!result.followed_artists.pending);
        assert!(!result.followed_artists.requires_refresh);
        assert!(server.calls("/v1/").is_empty());
    });
}
