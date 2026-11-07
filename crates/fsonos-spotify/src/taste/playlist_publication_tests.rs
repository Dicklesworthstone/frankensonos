//! Competing readers share a real SQLite cache while Spotify HTTP is in flight.

use super::*;
use crate::cache::{sync_library_attempt, to_entry};
use crate::classical::CandidatePool;
use fsonos_core::store::{
    LibraryOrigin, PlaylistTrack, SpotifyLibraryOwner, SpotifyPlaylistPublication,
    SpotifyPlaylistPublicationFence,
};

fn prepare_competing_window(
    store: &mut SqliteStore,
    change: impl FnOnce(&mut Value),
) -> (SpotifyLibraryOwner, SpotifyPlaylistPublication) {
    let owner = store.spotify_library_owner().unwrap().unwrap();
    let previous = store.playlist_progress(OWNER, URI).unwrap().unwrap();
    let mut progress: Value = serde_json::from_str(&previous).unwrap();
    change(&mut progress);
    progress["applied"] = json!(false);
    progress["fetched_at"] = json!(progress["fetched_at"].as_i64().unwrap_or(0) + 1);
    let encoded = progress.to_string();
    assert!(
        store
            .save_spotify_playlist_progress(&owner, URI, Some(&previous), &encoded)
            .unwrap()
    );
    let mut items = Vec::new();
    let occurrences = progress["tracks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            let mut item: LibraryItem = serde_json::from_value(row["item"].clone()).unwrap();
            item.taste_pm = Some(crate::taste::PLAYLIST_PM);
            let track = item.to_track();
            let occurrence = PlaylistTrack {
                position: u32::try_from(row["position"].as_u64().unwrap()).unwrap(),
                source_uri: track.source_uri,
                title: track.title,
                artist: track.artist,
                album: track.album,
                album_uri: item.album_uri.clone(),
                disc_number: item.disc_number,
                track_number: item.track_number,
                duration_secs: item.duration_secs,
                explicit: item.explicit,
            };
            items.push(item);
            occurrence
        })
        .collect();
    let observed = CachedPlaylist::new(
        URI.into(),
        progress["fetched_at"].as_i64().unwrap(),
        Some(u32::try_from(progress["source_total"].as_u64().unwrap()).unwrap()),
        u32::try_from(progress["examined"].as_u64().unwrap()).unwrap(),
        progress["complete"].as_bool().unwrap(),
        occurrences,
    );
    let observation = crate::taste::PlaylistObservation {
        contents: observed,
        snapshot_id: progress["snapshot_id"].as_str().unwrap().into(),
        progress: Some(encoded),
        applied: false,
        failed: false,
    };
    let directory = store.spotify_playlist_directory(&owner).unwrap();
    let (fence, _) = crate::taste::prepare_playlist_publication(
        &owner,
        None,
        directory.as_deref(),
        &[observation],
    )
    .unwrap();
    let pool = CandidatePool::build(&items);
    let entries = items
        .iter()
        .map(|item| to_entry(item, pool.get(&item.source_uri)))
        .collect();
    (
        owner,
        SpotifyPlaylistPublication {
            expected_directory: fence.expected_directory,
            directory: fence.directory,
            playlists: None,
            entries,
            observations: fence.observations,
        },
    )
}

#[test]
fn a_late_playlist_window_cannot_overwrite_a_newer_full_sync_or_its_proof() {
    let server = Server::new();
    populate(&server, "stable", 405, 0);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let file = server.data.join("playlist-full-publication-race.db");
        let mut store = SqliteStore::open(&file).unwrap();
        full(&server, &cx, &mut store).await;
        let other_file = file.clone();
        server.state.lock().unwrap().before_response = Some((
            format!("/v1/playlists/{ID}/items"),
            Box::new(move || {
                let mut other = SqliteStore::open(&other_file).unwrap();
                let (owner, mut publication) = prepare_competing_window(&mut other, |progress| {
                    progress["snapshot_id"] = json!("newer-snapshot");
                    let item = &mut progress["tracks"][0]["item"];
                    item["title"] = json!("New saved metadata");
                    item["explicit"] = json!(true);
                    item["artist_id"] = json!("NewestArtist");
                    item["genres"] = json!(["new genre"]);
                });
                let current = publication
                    .entries
                    .iter_mut()
                    .find(|entry| entry.track.source_uri == "spotify:track:TestTrack0")
                    .unwrap();
                current.origin = LibraryOrigin::LikedTrack;
                current.added = 777;
                current.taste_weight = Some(1136);
                let marker: crate::client::FullTrack = serde_json::from_value(track(900)).unwrap();
                let marker = marker.library_item(Origin::LikedTrack, None).unwrap();
                let pool = CandidatePool::build(std::slice::from_ref(&marker));
                publication
                    .entries
                    .push(to_entry(&marker, pool.get(&marker.source_uri)));
                let observed = publication.observations[0].contents.clone();
                let fence = SpotifyPlaylistPublicationFence {
                    expected_directory: publication.expected_directory,
                    directory: publication.directory,
                    observations: publication.observations,
                };
                assert!(
                    other
                        .save_spotify_library_progress(OWNER, Some(&owner), None, "winning-full")
                        .unwrap()
                );
                assert!(
                    other
                        .publish_spotify_library_from_progress(
                            &owner,
                            "winning-full",
                            &publication.entries,
                            None,
                            &[observed],
                            None,
                            Some(&fence),
                        )
                        .unwrap()
                );
                other.close().unwrap();
            }),
        ));
        let result = pending(&server, &cx, &mut store).await;
        assert!(result.playlists.requires_refresh);
        assert_eq!((result.library.tracks, result.library.retired), (151, 0));
        assert!(
            result
                .notes
                .iter()
                .any(|note| note.contains("checkpoint changed"))
        );
        let kept = store
            .library()
            .unwrap()
            .into_iter()
            .find(|entry| entry.track.source_uri == "spotify:track:TestTrack0")
            .unwrap();
        assert_eq!(kept.track.title, "New saved metadata");
        assert!(kept.explicit);
        assert_eq!(kept.origin, LibraryOrigin::LikedTrack);
        assert_eq!(kept.artist_id.as_deref(), Some("NewestArtist"));
        assert_eq!(kept.taste_weight, Some(1136));
        assert_eq!(kept.genres, ["new genre"]);
        let pool = pool_from_store(&store).unwrap();
        assert!(pool.get("spotify:track:TestTrack900").is_some());
        assert!(pool.get("spotify:track:TestTrack150").is_none());
        assert_eq!(contents(&store).tracks[0].title, "New saved metadata");
        let proof = store.playlist_progress(OWNER, URI).unwrap().unwrap();
        let proof: Value = serde_json::from_str(&proof).unwrap();
        assert_eq!(proof["snapshot_id"], "newer-snapshot");
        assert_eq!(proof["applied"], true);
        store.close().unwrap();
    });
}

#[test]
fn a_full_sync_cannot_retire_a_background_window_accepted_during_genre_http() {
    let server = Server::new();
    populate(&server, "stable", 405, 0);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let file = server.data.join("full-playlist-publication-race.db");
        let mut store = SqliteStore::open(&file).unwrap();
        full(&server, &cx, &mut store).await;
        let mut liked = track(900);
        liked["artists"][0]["id"] = json!("GenreGate");
        server.set(
            "/v1/me/tracks",
            page(vec![json!({ "track": liked })], 0, None),
        );
        server.set(
            "/v1/artists/GenreGate",
            json!({ "id": "GenreGate", "name": "Test artist", "genres": ["jazz"] }),
        );
        let other_file = file.clone();
        server.state.lock().unwrap().before_response = Some((
            "/v1/artists/GenreGate".into(),
            Box::new(move || {
                let mut other = SqliteStore::open(&other_file).unwrap();
                let (owner, publication) = prepare_competing_window(&mut other, |progress| {
                    assert_eq!(progress["examined"], 300);
                    let rows = progress["tracks"].as_array_mut().unwrap();
                    for number in 300..405 {
                        let track: crate::client::FullTrack =
                            serde_json::from_value(track(number)).unwrap();
                        let item = track.library_item(Origin::Taste, None).unwrap();
                        rows.push(json!({ "position": number + 1, "item": item }));
                    }
                    progress["examined"] = json!(405);
                    progress["complete"] = json!(true);
                });
                // The same directory and snapshot IDs now cover a later bounded
                // subset. Only the publication receipt itself must change.
                assert!(
                    other
                        .publish_spotify_playlist_continuation(&owner, &publication)
                        .unwrap()
                );
                other.close().unwrap();
            }),
        ));
        let mut session = server.session(&cx, PLAYLIST_SCOPE);
        let attempt = sync_library_attempt(&mut session, &cx, &mut store).await;
        assert!(attempt.result.is_err());
        assert!(
            attempt.progress.is_some(),
            "the saved-library proof remains ready to retry"
        );
        assert!(
            server.state.lock().unwrap().before_response.is_none(),
            "the race hook ran"
        );
        let owner = store.spotify_library_owner().unwrap().unwrap();
        assert!(
            store
                .spotify_library_progress(OWNER, Some(&owner))
                .unwrap()
                .is_some()
        );
        assert_eq!(pool_from_store(&store).unwrap().len(), 405);
        assert!(
            pool_from_store(&store)
                .unwrap()
                .get("spotify:track:TestTrack404")
                .is_some()
        );
        assert!(contents(&store).complete);
        assert_eq!(contents(&store).examined, 405);
        store.close().unwrap();

        // Restart retains both the accepted playlist and the unpublished saved
        // proof; the retry can reconcile their union without losing either.
        let mut store = SqliteStore::open(&file).unwrap();
        let completed = full(&server, &cx, &mut store).await;
        assert_eq!(
            (completed.library.tracks, completed.library.retired),
            (406, 0)
        );
        assert!(
            store
                .spotify_library_progress(OWNER, Some(&owner))
                .unwrap()
                .is_none()
        );
        assert!(
            pool_from_store(&store)
                .unwrap()
                .get("spotify:track:TestTrack900")
                .is_some()
        );
        assert!(
            pool_from_store(&store)
                .unwrap()
                .get("spotify:track:TestTrack404")
                .is_some()
        );
        store.close().unwrap();
    });
}

#[test]
fn an_unadopted_taste_read_cannot_replace_a_concurrently_adopted_checkpoint() {
    let server = Server::new();
    populate(&server, "stable", 3, 0);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let file = server.data.join("unadopted-playlist-race.db");
        let mut store = SqliteStore::open(&file).unwrap();
        let other_file = file.clone();
        server.state.lock().unwrap().before_response = Some((
            format!("/v1/playlists/{ID}/items"),
            Box::new(move || {
                let mut other = SqliteStore::open(&other_file).unwrap();
                let (owner, _) = other.adopt_spotify_library_owner(OWNER).unwrap();
                assert!(
                    other
                        .save_spotify_playlist_progress(&owner, URI, None, "new owner's proof")
                        .unwrap()
                );
                other.close().unwrap();
            }),
        ));
        let mut session = server.session(&cx, PLAYLIST_SCOPE);
        let _ = read_taste(&mut session, &cx, &mut store, &[], 42).await;
        assert_eq!(
            store.playlist_progress(OWNER, URI).unwrap().as_deref(),
            Some("new owner's proof")
        );
        assert!(store.library().unwrap().is_empty());
        assert!(store.playlist_contents(URI).unwrap().is_none());
        store.close().unwrap();
    });
}
