//! Resumed catalog pages must still describe the album being published.

use super::*;

fn corrected_page(server: &Server, offset: u32, total: u32) {
    let mut corrected = movement(ALBUM, offset + 1);
    corrected["explicit"] = json!(true);
    corrected["duration_ms"] = json!(510_000);
    server.set(
        &format!("{ROUTE}?offset={offset}"),
        json!({
            "items": [corrected], "offset": offset, "total": total,
            "next": (offset + 1 < total).then(|| {
                format!("{API}/albums/{ALBUM}/tracks?offset={}", offset + 1)
            })
        }),
    );
}

#[test]
fn same_total_root_or_middle_changes_restart_before_publishing_an_album() {
    for changed_offset in [0, 3] {
        let server = Server::new();
        album_pages(&server, ALBUM, 10);
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let file = server.data.join("album-drift.db");
            let mut store = SqliteStore::open(&file).unwrap();
            store.adopt_spotify_library_owner(OWNER).unwrap();
            apply_library_read(&mut store, &[held(ALBUM)]).unwrap();
            let first = continue_with(&server, &cx, &mut store, None).await;
            let saved = serde_json::to_string(&first.albums).unwrap();
            let library = store.library().unwrap();
            store.close().unwrap();

            corrected_page(&server, changed_offset, 10);
            let previous = serde_json::from_str(&saved).unwrap();
            let mut store = SqliteStore::open(&file).unwrap();
            let before = server.calls(ROUTE).len();
            let changed = continue_with(&server, &cx, &mut store, Some(&previous)).await;
            assert_eq!(changed.albums.completed, 0);
            assert!(changed.notes.iter().any(|note| note.contains("catalog changed")));
            assert!(server.calls(ROUTE).len() - before <= ALBUM_PAGE_BUDGET);
            let uri = format!("spotify:album:{ALBUM}");
            assert!(store.album_tracks(&uri).unwrap().is_none());
            assert_eq!(store.library().unwrap(), library);
            let done = finish_with(&server, &cx, &mut store, &changed.albums).await;
            assert_eq!((done.albums.completed, done.albums.pending), (1, 0));
            let album = store.album_tracks(&uri).unwrap().unwrap();
            let changed = &album.tracks[changed_offset as usize];
            assert_eq!(changed.explicit, Some(true));
            assert_eq!(changed.duration_secs, Some(510));
            assert_eq!(album.tracks.len(), 10);
            assert_eq!(store.library().unwrap(), library);
            store.close().unwrap();
        });
    }
}

#[test]
fn a_shrunken_root_restarts_from_zero_without_fetching_removed_offsets() {
    let server = Server::new();
    album_pages(&server, ALBUM, 10);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        store.adopt_spotify_library_owner(OWNER).unwrap();
        apply_library_read(&mut store, &[held(ALBUM)]).unwrap();
        let first = continue_with(&server, &cx, &mut store, None).await;
        album_pages(&server, ALBUM, 3);
        corrected_page(&server, 0, 3);
        let before = server.calls(ROUTE).len();
        let changed = continue_with(&server, &cx, &mut store, Some(&first.albums)).await;
        assert_eq!((changed.albums.completed, changed.albums.pending), (0, 1));
        assert_eq!(server.calls(ROUTE).len() - before, 1, "changed root is discarded");
        let done = continue_with(&server, &cx, &mut store, Some(&changed.albums)).await;
        assert_eq!((done.albums.completed, done.albums.pending), (1, 0));
        let calls = server.calls(ROUTE);
        assert_eq!(calls.len() - before, 4, "root check plus a fresh three-page read");
        assert!(
            calls[before..]
                .iter()
                .all(|call| !call.contains("offset=8") && !call.contains("offset=9"))
        );
        let album = store.album_tracks(&format!("spotify:album:{ALBUM}")).unwrap().unwrap();
        assert_eq!(album.tracks.len(), 3);
        assert_eq!(album.tracks[0].explicit, Some(true));
    });
}

#[test]
fn root_outages_keep_the_verification_cursor_and_publish_no_partial_album() {
    for status in [503, 429] {
        let server = Server::new();
        album_pages(&server, ALBUM, 10);
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let file = server.data.join("album-proof-outage.db");
            let mut store = SqliteStore::open(&file).unwrap();
            let (owner, _) = store.adopt_spotify_library_owner(OWNER).unwrap();
            apply_library_read(&mut store, &[held(ALBUM)]).unwrap();
            let first = continue_with(&server, &cx, &mut store, None).await;
            let checking = continue_with(&server, &cx, &mut store, Some(&first.albums)).await;
            let saved = serde_json::to_value(&checking.albums).unwrap();
            assert_eq!(saved["partial"]["checked"], json!(6));
            assert_eq!(saved["partial"]["complete"], json!(true));
            let library = store.library().unwrap();
            store.close().unwrap();

            server.state.lock().unwrap().refused.insert(ROUTE.into(), status);
            let previous: AlbumContinuation = serde_json::from_value(saved.clone()).unwrap();
            let mut store = SqliteStore::open(&file).unwrap();
            let before = server.calls(ROUTE).len();
            let stopped = continue_with(&server, &cx, &mut store, Some(&previous)).await;
            assert_eq!(stopped.albums.completed, 0);
            assert_eq!(stopped.albums.advanced, 0);
            assert_eq!(server.calls(ROUTE).len() - before, 1);
            assert_eq!(
                serde_json::to_value(&stopped.albums).unwrap()["partial"],
                saved["partial"],
                "failed root observation cannot consume verified positions"
            );
            assert_eq!(store.spotify_library_owner().unwrap(), Some(owner));
            assert_eq!(store.library().unwrap(), library);
            assert!(store.album_tracks(&format!("spotify:album:{ALBUM}")).unwrap().is_none());
            if status == 429 {
                assert!(stopped.albums.retry_after_secs > 0);
            }
            server.state.lock().unwrap().refused.remove(ROUTE);
            let done = finish_with(&server, &cx, &mut store, &stopped.albums).await;
            assert_eq!((done.albums.completed, done.albums.pending), (1, 0));
            store.close().unwrap();
        });
    }
}

#[test]
fn legacy_expired_or_modified_partial_metadata_is_refetched_from_zero() {
    for change in ["legacy", "expired", "metadata"] {
        let server = Server::new();
        album_pages(&server, ALBUM, 10);
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let mut store = MemStore::default();
            store.adopt_spotify_library_owner(OWNER).unwrap();
            apply_library_read(&mut store, &[held(ALBUM)]).unwrap();
            let first = continue_with(&server, &cx, &mut store, None).await;
            let mut saved = serde_json::to_value(&first.albums).unwrap();
            match change {
                "legacy" => {
                    saved["partial"].as_object_mut().unwrap().remove("proofs");
                }
                "expired" => saved["partial"]["started_at"] = json!(1),
                _ => saved["partial"]["tracks"][0]["duration_secs"] = json!(999),
            }
            let previous: AlbumContinuation = serde_json::from_value(saved).unwrap();
            let before = server.calls(ROUTE).len();
            let restarted = continue_with(&server, &cx, &mut store, Some(&previous)).await;
            assert_eq!(restarted.albums.advanced, ALBUM_PAGE_BUDGET);
            assert_eq!(server.calls(ROUTE).len() - before, ALBUM_PAGE_BUDGET);
            assert!(server.calls(ROUTE)[before].contains("offset=0"));
            assert!(store.album_tracks(&format!("spotify:album:{ALBUM}")).unwrap().is_none());
            let done = finish_with(&server, &cx, &mut store, &restarted.albums).await;
            assert_eq!(done.albums.completed, 1);
            let album = store.album_tracks(&format!("spotify:album:{ALBUM}")).unwrap().unwrap();
            assert!(album.tracks.iter().all(|track| track.duration_secs == Some(180)));
        });
    }
}

#[test]
fn verification_cannot_publish_after_an_account_changes_away_and_back() {
    let server = Server::new();
    album_pages(&server, ALBUM, 10);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let file = server.data.join("album-verification-owner.db");
        let mut store = SqliteStore::open(&file).unwrap();
        let (owner, _) = store.adopt_spotify_library_owner(OWNER).unwrap();
        apply_library_read(&mut store, &[held(ALBUM)]).unwrap();
        let first = continue_with(&server, &cx, &mut store, None).await;
        let checking = continue_with(&server, &cx, &mut store, Some(&first.albums)).await;
        assert_eq!(serde_json::to_value(&checking.albums).unwrap()["partial"]["checked"], json!(6));
        server.state.lock().unwrap().before_response = Some((
            ROUTE.into(),
            Box::new(move || {
                let mut other = SqliteStore::open(&file).unwrap();
                other.adopt_spotify_library_owner("another-test-owner").unwrap();
                let (returned, _) = other.adopt_spotify_library_owner(OWNER).unwrap();
                assert_ne!(returned.generation, owner.generation);
                apply_library_read(&mut other, &[held(ALBUM)]).unwrap();
                other.close().unwrap();
            }),
        ));
        let before = server.calls(ROUTE).len();
        let changed = continue_with(&server, &cx, &mut store, Some(&checking.albums)).await;
        assert!(changed.albums.requires_refresh);
        assert_eq!(changed.albums.completed, 0);
        assert_eq!(server.calls(ROUTE).len() - before, 1);
        assert!(serde_json::to_value(&changed.albums).unwrap()["partial"].is_null());
        assert!(store.album_tracks(&format!("spotify:album:{ALBUM}")).unwrap().is_none());
        store.close().unwrap();
    });
}

#[test]
fn a_changing_partial_yields_its_remaining_budget_to_stable_albums() {
    let server = Server::new();
    let stable = "ZStableSonata";
    album_pages(&server, ALBUM, 10);
    album_pages(&server, stable, 3);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        store.adopt_spotify_library_owner(OWNER).unwrap();
        apply_library_read(&mut store, &[held(ALBUM), held(stable)]).unwrap();
        let first = continue_with(&server, &cx, &mut store, None).await;
        assert_eq!(first.albums.pending, 2);
        assert_eq!(first.albums.completed, 0);
        corrected_page(&server, 0, 10);
        let before = server.calls("/v1/albums/").len();
        let changed = continue_with(&server, &cx, &mut store, Some(&first.albums)).await;
        assert_eq!((changed.albums.completed, changed.albums.pending), (1, 1));
        assert_eq!(
            server.calls("/v1/albums/").len() - before, 4,
            "one rejected root plus the stable album's three pages"
        );
        assert!(store.album_tracks(&format!("spotify:album:{ALBUM}")).unwrap().is_none());
        assert_eq!(
            store.album_tracks(&format!("spotify:album:{stable}")).unwrap().unwrap()
                .tracks.len(),
            3
        );
        assert!(serde_json::to_value(&changed.albums).unwrap()["partial"].is_null());
    });
}
