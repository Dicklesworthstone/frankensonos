//! Every cache entry point shares the catalog page budget and durable cursor.

use super::*;
use crate::cache::{continue_playlists_with_albums, sync_library_detailed_with_albums};

fn liked(server: &Server, albums: &[&str]) {
    server.set(
        "/v1/me/tracks",
        page(
            albums
                .iter()
                .map(|album| json!({ "track": movement(album, 2) }))
                .collect(),
            0,
            None,
        ),
    );
}

#[test]
fn full_sync_bounds_album_pages_and_background_recovery_resumes_after_restart() {
    let server = Server::new();
    liked(&server, &[ALBUM]);
    album_pages(&server, ALBUM, 10);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let file = server.data.join("full-bounded-albums.db");
        let mut store = SqliteStore::open(&file).unwrap();
        let mut session = server.session(&cx, "user-library-read");
        let first = sync_library_detailed(&mut session, &cx, &mut store)
            .await
            .unwrap();
        assert_eq!(
            first.library.tracks, 1,
            "the essential library is already usable"
        );
        assert_eq!(server.calls(ROUTE).len(), ALBUM_PAGE_BUDGET);
        assert_eq!((first.albums.completed, first.albums.pending), (0, 1));
        assert_eq!(first.albums.advanced, ALBUM_PAGE_BUDGET);
        assert!(
            store
                .album_tracks(&format!("spotify:album:{ALBUM}"))
                .unwrap()
                .is_none()
        );
        let accepted = store.library().unwrap();
        let saved = serde_json::to_string(&first.albums).unwrap();
        store.close().unwrap();

        let mut store = SqliteStore::open(&file).unwrap();
        let previous: AlbumContinuation = serde_json::from_str(&saved).unwrap();
        let done = finish_with(&server, &cx, &mut store, &previous).await;
        assert_eq!((done.albums.completed, done.albums.pending), (1, 0));
        let calls = server.calls(ROUTE);
        assert_eq!(
            calls.len(),
            21,
            "retained pages were verified before publication"
        );
        assert!(calls[ALBUM_PAGE_BUDGET].contains("offset=0"));
        assert!(calls[ALBUM_PAGE_BUDGET + 1].contains("offset=8"));
        assert_eq!(store.library().unwrap(), accepted);
        assert_eq!(
            works_from_store(&store)
                .unwrap()
                .work_of(&held(ALBUM).source_uri)
                .unwrap()
                .movements
                .len(),
            10
        );
        store.close().unwrap();
    });
}

#[test]
fn a_full_refresh_uses_the_previous_album_offset_instead_of_restarting_it() {
    let server = Server::new();
    liked(&server, &[ALBUM]);
    album_pages(&server, ALBUM, 10);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let file = server.data.join("full-resumed-albums.db");
        let mut store = SqliteStore::open(&file).unwrap();
        store.adopt_spotify_library_owner(OWNER).unwrap();
        apply_library_read(&mut store, &[held(ALBUM)]).unwrap();
        let first = continue_with(&server, &cx, &mut store, None).await;
        let saved = serde_json::to_string(&first.albums).unwrap();
        store.close().unwrap();

        let previous: AlbumContinuation = serde_json::from_str(&saved).unwrap();
        let mut store = SqliteStore::open(&file).unwrap();
        let mut session = server.session(&cx, "user-library-read");
        let checking =
            sync_library_detailed_with_albums(&mut session, &cx, &mut store, Some(&previous))
                .await
                .unwrap();
        assert_eq!((checking.albums.completed, checking.albums.pending), (0, 1));
        assert_eq!(checking.albums.advanced, ALBUM_PAGE_BUDGET);
        assert_eq!(server.calls(ROUTE).len(), 2 * ALBUM_PAGE_BUDGET);
        let done = sync_library_detailed_with_albums(
            &mut session,
            &cx,
            &mut store,
            Some(&checking.albums),
        )
        .await
        .unwrap();
        assert_eq!((done.albums.completed, done.albums.pending), (1, 0));
        assert_eq!(server.calls(ROUTE).len(), 21);
        let mut accepted = done.albums.clone();
        accepted.inherit_progress(&previous);
        assert_eq!(
            accepted, done.albums,
            "the scheduler must not resurrect completed pages"
        );
        store.close().unwrap();
    });
}

#[test]
fn playlist_windows_bound_album_work_and_resume_without_an_essential_read() {
    let server = Server::new();
    server.set("/v1/me/playlists", page(vec![playlist(0, true)], 0, None));
    server.set(
        "/v1/playlists/TestList0/items",
        page(vec![json!({ "item": movement(ALBUM, 2) })], 0, None),
    );
    album_pages(&server, ALBUM, 10);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let file = server.data.join("playlist-bounded-albums.db");
        let mut store = SqliteStore::open(&file).unwrap();
        let (owner, _) = store.adopt_spotify_library_owner(OWNER).unwrap();
        assert!(
            store
                .save_spotify_library_progress(OWNER, Some(&owner), None, "another-saved-read")
                .unwrap()
        );
        let mut session = server.session(&cx, PLAYLIST_SCOPE);
        let first = continue_playlists_with_albums(&mut session, &cx, &mut store, OWNER, None)
            .await
            .unwrap();
        assert_eq!(first.library.tracks, 1);
        assert_eq!(server.calls(ROUTE).len(), ALBUM_PAGE_BUDGET);
        assert_eq!((first.albums.completed, first.albums.pending), (0, 1));
        assert_eq!(first.albums.advanced, ALBUM_PAGE_BUDGET);
        assert!(
            store
                .album_tracks(&format!("spotify:album:{ALBUM}"))
                .unwrap()
                .is_none()
        );
        let saved = serde_json::to_string(&first.albums).unwrap();
        store.close().unwrap();

        let previous: AlbumContinuation = serde_json::from_str(&saved).unwrap();
        let mut store = SqliteStore::open(&file).unwrap();
        let mut session = server.session(&cx, PLAYLIST_SCOPE);
        let checking =
            continue_playlists_with_albums(&mut session, &cx, &mut store, OWNER, Some(&previous))
                .await
                .unwrap();
        assert_eq!((checking.albums.completed, checking.albums.pending), (0, 1));
        assert_eq!(checking.albums.advanced, ALBUM_PAGE_BUDGET);
        assert_eq!(server.calls(ROUTE).len(), 2 * ALBUM_PAGE_BUDGET);
        let done = continue_playlists_with_albums(
            &mut session,
            &cx,
            &mut store,
            OWNER,
            Some(&checking.albums),
        )
        .await
        .unwrap();
        assert_eq!((done.albums.completed, done.albums.pending), (1, 0));
        assert_eq!(server.calls(ROUTE).len(), 21);
        assert!(server.calls("/v1/me/albums").is_empty());
        assert!(server.calls("/v1/me/tracks").is_empty());
        assert_eq!(
            store
                .spotify_library_progress(OWNER, Some(&owner))
                .unwrap()
                .as_deref(),
            Some("another-saved-read")
        );
        store.close().unwrap();
    });
}

#[test]
fn the_page_allowance_is_shared_across_all_albums_in_a_full_refresh() {
    let server = Server::new();
    let albums: Vec<String> = (0..10)
        .map(|number| format!("BudgetAlbum{number:02}"))
        .collect();
    liked(
        &server,
        &albums.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    for album in &albums {
        server.set(
            &format!("/v1/albums/{album}/tracks"),
            page(
                (1..=3).map(|number| movement(album, number)).collect(),
                0,
                None,
            ),
        );
    }
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        let mut session = server.session(&cx, "user-library-read");
        let first = sync_library_detailed(&mut session, &cx, &mut store)
            .await
            .unwrap();
        assert_eq!(server.calls("/v1/albums/").len(), ALBUM_PAGE_BUDGET);
        assert_eq!((first.albums.completed, first.albums.pending), (8, 2));
        let done =
            sync_library_detailed_with_albums(&mut session, &cx, &mut store, Some(&first.albums))
                .await
                .unwrap();
        assert_eq!((done.albums.completed, done.albums.pending), (2, 0));
        assert_eq!(server.calls("/v1/albums/").len(), 10);
    });
}

#[test]
fn one_malformed_album_does_not_block_other_works_in_a_full_refresh() {
    let server = Server::new();
    liked(&server, &["BadAlbum", "GoodAlbum"]);
    let mut missing_flag = movement("BadAlbum", 1);
    missing_flag.as_object_mut().unwrap().remove("explicit");
    server.set(
        "/v1/albums/BadAlbum/tracks",
        page(vec![missing_flag], 0, None),
    );
    server.set(
        "/v1/albums/GoodAlbum/tracks",
        page(
            (1..=3)
                .map(|number| movement("GoodAlbum", number))
                .collect(),
            0,
            None,
        ),
    );
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = MemStore::default();
        let mut session = server.session(&cx, "user-library-read");
        let done = sync_library_detailed(&mut session, &cx, &mut store)
            .await
            .unwrap();
        assert_eq!((done.albums.completed, done.albums.pending), (1, 1));
        assert_eq!(server.calls("/v1/albums/").len(), 2);
        assert!(done.notes.iter().any(|note| note.contains("BadAlbum")));
        assert!(
            store
                .album_tracks("spotify:album:BadAlbum")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store
                .album_tracks("spotify:album:GoodAlbum")
                .unwrap()
                .unwrap()
                .tracks
                .len(),
            3
        );
        assert_eq!(done.library.tracks, 2);
    });
}

#[test]
fn owner_changes_during_post_library_album_reads_cannot_publish_old_catalog_work() {
    let server = Server::new();
    liked(&server, &[ALBUM]);
    album_pages(&server, ALBUM, 3);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let file = server.data.join("full-album-owner.db");
        let mut store = SqliteStore::open(&file).unwrap();
        server.state.lock().unwrap().before_response = Some((
            ROUTE.into(),
            Box::new(move || {
                let mut other = SqliteStore::open(&file).unwrap();
                other
                    .adopt_spotify_library_owner("another-test-owner")
                    .unwrap();
                other.close().unwrap();
            }),
        ));
        let mut session = server.session(&cx, "user-library-read");
        let done = sync_library_detailed(&mut session, &cx, &mut store)
            .await
            .unwrap();
        assert!(done.albums.requires_refresh);
        assert_eq!(done.albums.completed, 0);
        assert_eq!(server.calls(ROUTE).len(), 1);
        assert!(
            store
                .album_tracks(&format!("spotify:album:{ALBUM}"))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store.spotify_library_owner().unwrap().unwrap().owner_id,
            "another-test-owner"
        );
        store.close().unwrap();
    });
}

#[test]
fn a_missing_playlist_grant_or_unproved_identity_cannot_start_unrelated_album_reads() {
    for granted in [false, true] {
        let server = Server::new();
        album_pages(&server, ALBUM, 3);
        if granted {
            server
                .state
                .lock()
                .unwrap()
                .refused
                .insert("/v1/me".into(), 403);
        }
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let mut store = MemStore::default();
            store.adopt_spotify_library_owner(OWNER).unwrap();
            apply_library_read(&mut store, &[held(ALBUM)]).unwrap();
            let mut session = server.session(&cx, if granted { PLAYLIST_SCOPE } else { "" });
            let done = continue_playlists_with_albums(&mut session, &cx, &mut store, OWNER, None)
                .await
                .unwrap();
            assert_eq!(done.albums.pending, 1);
            assert_eq!(done.albums.completed, 0);
            assert!(server.calls("/v1/albums/").is_empty());
        });
    }
}
