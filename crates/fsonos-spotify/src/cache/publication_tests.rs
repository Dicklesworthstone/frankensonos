//! Additive playlist publication and essential replacement have different proofs.

use super::*;
use crate::taste::PlaylistObservation;
use fsonos_core::store::{MemStore, PlaylistTrack};
use serde_json::json;

const OWNER: &str = "test-library-owner";
const PLAYLIST: &str = "spotify:playlist:Publication";

fn song(number: u32) -> LibraryItem {
    LibraryItem {
        source_uri: format!("spotify:track:Publication{number}"),
        title: format!("Song {number}"),
        artists: vec!["Test artist".into()],
        origin: Origin::LikedTrack,
        ..LibraryItem::default()
    }
}

fn stage_playlist(store: &mut dyn Store, owner: &SpotifyLibraryOwner, numbers: &[u32]) -> Taste {
    let mut taste = Taste::default();
    taste.tracks = numbers
        .iter()
        .map(|&number| {
            let mut item = song(number);
            item.origin = Origin::Taste;
            item.taste_pm = Some(1030);
            item
        })
        .collect();
    let count = u32::try_from(numbers.len()).unwrap();
    let progress = json!({
        "version": 1, "owner_id": OWNER, "playlist_uri": PLAYLIST,
        "snapshot_id": "snapshot", "source_total": count, "examined": count,
        "complete": true, "applied": false, "fetched_at": 42,
        "tracks": taste.tracks.iter().enumerate().map(|(position, item)| {
            json!({ "position": position + 1, "item": item })
        }).collect::<Vec<_>>()
    })
    .to_string();
    store
        .save_playlist_progress(OWNER, PLAYLIST, &progress)
        .unwrap();
    let contents = CachedPlaylist::new(
        PLAYLIST.into(),
        42,
        Some(count),
        count,
        true,
        taste
            .tracks
            .iter()
            .enumerate()
            .map(|(position, item)| {
                let track = item.to_track();
                PlaylistTrack {
                    position: u32::try_from(position + 1).unwrap(),
                    source_uri: track.source_uri,
                    title: track.title,
                    artist: track.artist,
                    album: track.album,
                    album_uri: item.album_uri.clone(),
                    disc_number: item.disc_number,
                    track_number: item.track_number,
                    duration_secs: item.duration_secs,
                    explicit: item.explicit,
                }
            })
            .collect(),
    );
    taste.playlist_contents.push(PlaylistObservation {
        contents,
        snapshot_id: "snapshot".into(),
        progress: Some(progress),
        applied: false,
        failed: false,
    });
    let directory = json!({
        "version": 1, "owner_id": OWNER, "generation": owner.generation,
        "examined": 1, "source_total": 1, "first_end": 1, "last_start": 0,
        "complete": true, "limited": false, "reconcile": true,
        "items": [{ "id": "Publication", "name": "Test playlist", "collaborative": false,
            "owner": { "id": OWNER }, "snapshot_id": "snapshot" }]
    })
    .to_string();
    let expected = store.spotify_playlist_directory(owner).unwrap();
    assert!(
        store
            .save_spotify_playlist_directory(owner, expected.as_deref(), &directory, None)
            .unwrap()
    );
    let (fence, receipt) =
        crate::taste::prepare_playlist_publication(owner, None, Some(&directory), &[]).unwrap();
    assert!(
        store
            .save_spotify_playlist_directory(
                owner,
                Some(&directory),
                fence.directory.as_deref().unwrap(),
                None
            )
            .unwrap()
    );
    taste.directory = receipt;
    taste.playlists = Some(vec![(PLAYLIST.into(), "Test playlist".into())]);
    taste
}

#[test]
fn additive_publication_preserves_active_saved_metadata_and_an_essential_proof() {
    for proof in [None, Some("another-essential-read")] {
        let mut store = MemStore::default();
        let (owner, _) = store.adopt_spotify_library_owner(OWNER).unwrap();
        let mut taste = stage_playlist(&mut store, &owner, &[1, 2]);
        let mut latest = song(1);
        latest.title = "New saved-track title".into();
        latest.artist_id = Some("LatestArtist".into());
        latest.explicit = true;
        latest.genres = vec!["new genre".into()];
        latest.taste_pm = Some(1136);
        // The accepted rows can change after the playlist read was prepared.
        apply_library_read(&mut store, &[latest.clone(), song(3)]).unwrap();
        let before = store
            .library()
            .unwrap()
            .into_iter()
            .find(|entry| entry.track.source_uri == latest.source_uri)
            .unwrap();
        if let Some(proof) = proof {
            assert!(
                store
                    .save_spotify_library_progress(OWNER, Some(&owner), None, proof)
                    .unwrap()
            );
        }
        assert!(playlists::publish_playlist_read(&mut store, &owner, &mut taste).unwrap());
        assert_eq!(
            store
                .library()
                .unwrap()
                .into_iter()
                .find(|entry| entry.track.source_uri == latest.source_uri)
                .unwrap(),
            before
        );
        assert_eq!(
            store
                .spotify_library_progress(OWNER, Some(&owner))
                .unwrap()
                .as_deref(),
            proof
        );
        let pool = pool_from_store(&store).unwrap();
        assert_eq!(pool.len(), 3);
        assert!(pool.get(&song(2).source_uri).is_some());
        assert!(pool.get(&song(3).source_uri).is_some());
        let checkpoint: serde_json::Value =
            serde_json::from_str(&store.playlist_progress(OWNER, PLAYLIST).unwrap().unwrap())
                .unwrap();
        assert_eq!(checkpoint["applied"], true);
    }
}

#[test]
fn a_full_publication_invalidates_an_older_continuation_without_consuming_its_new_proof() {
    let mut store = MemStore::default();
    let (owner, _) = store.adopt_spotify_library_owner(OWNER).unwrap();
    let mut stale = stage_playlist(&mut store, &owner, &[1]);
    let directory = store.spotify_playlist_directory(&owner).unwrap();
    assert!(
        store
            .save_spotify_library_progress(OWNER, Some(&owner), None, "full")
            .unwrap()
    );
    assert!(
        publish_saved_library(
            &mut store,
            &owner,
            "full",
            &[song(9)],
            &mut Taste::default(),
            directory.as_deref()
        )
        .unwrap()
        .is_some()
    );
    assert!(
        store
            .save_spotify_library_progress(OWNER, Some(&owner), None, "new-read")
            .unwrap()
    );
    let before = store.library().unwrap();
    let checkpoint = store.playlist_progress(OWNER, PLAYLIST).unwrap();
    assert!(!playlists::publish_playlist_read(&mut store, &owner, &mut stale).unwrap());
    assert_eq!(store.library().unwrap(), before);
    assert_eq!(
        store.playlist_progress(OWNER, PLAYLIST).unwrap(),
        checkpoint
    );
    assert_eq!(
        store
            .spotify_library_progress(OWNER, Some(&owner))
            .unwrap()
            .as_deref(),
        Some("new-read")
    );
}

#[test]
fn essential_publication_requires_the_exact_read_and_playlist_directory() {
    let mut store = MemStore::default();
    let (owner, _) = store.adopt_spotify_library_owner(OWNER).unwrap();
    apply_library_read(&mut store, &[song(1)]).unwrap();
    let mut background = stage_playlist(&mut store, &owner, &[2]);
    let directory = store.spotify_playlist_directory(&owner).unwrap();
    assert!(
        store
            .save_spotify_library_progress(OWNER, Some(&owner), None, "current-read")
            .unwrap()
    );
    assert!(playlists::publish_playlist_read(&mut store, &owner, &mut background).unwrap());
    let before = store.library().unwrap();
    for (proof, directory) in [
        (
            "stale-read",
            store.spotify_playlist_directory(&owner).unwrap(),
        ),
        ("current-read", directory),
    ] {
        assert!(
            publish_saved_library(
                &mut store,
                &owner,
                proof,
                &[song(3)],
                &mut Taste::default(),
                directory.as_deref()
            )
            .unwrap()
            .is_none()
        );
        assert_eq!(store.library().unwrap(), before);
    }
    assert_eq!(
        store
            .spotify_library_progress(OWNER, Some(&owner))
            .unwrap()
            .as_deref(),
        Some("current-read")
    );
    let current = store.spotify_playlist_directory(&owner).unwrap();
    assert!(
        publish_saved_library(
            &mut store,
            &owner,
            "current-read",
            &[song(3)],
            &mut Taste::default(),
            current.as_deref()
        )
        .unwrap()
        .is_some()
    );
    assert!(
        store
            .spotify_library_progress(OWNER, Some(&owner))
            .unwrap()
            .is_none()
    );
    let pool = pool_from_store(&store).unwrap();
    assert_eq!(pool.len(), 1);
    assert!(pool.get(&song(3).source_uri).is_some());
}
