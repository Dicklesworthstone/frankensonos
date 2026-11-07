//! Additive playlist publication and essential replacement have different proofs.

use super::*;
use fsonos_core::store::MemStore;

const OWNER: &str = "test-library-owner";

fn song(number: u32) -> LibraryItem {
    LibraryItem {
        source_uri: format!("spotify:track:Publication{number}"),
        title: format!("Song {number}"),
        artists: vec!["Test artist".into()],
        origin: Origin::LikedTrack,
        ..LibraryItem::default()
    }
}

#[test]
fn additive_publication_needs_no_essential_proof_and_preserves_an_active_one() {
    for proof in [None, Some("another-essential-read")] {
        let mut store = MemStore::default();
        let (owner, _) = store.adopt_spotify_library_owner(OWNER).unwrap();
        apply_library_read(&mut store, &[song(1)]).unwrap();
        if let Some(proof) = proof {
            assert!(
                store
                    .save_spotify_library_progress(OWNER, Some(&owner), None, proof)
                    .unwrap()
            );
        }
        let result =
            publish_library_read(&mut store, &owner, &[song(1), song(2)], &Taste::default())
                .unwrap()
                .unwrap();
        assert_eq!((result.tracks, result.retired), (2, 0));
        assert_eq!(
            store
                .spotify_library_progress(OWNER, Some(&owner))
                .unwrap()
                .as_deref(),
            proof
        );
        assert_eq!(pool_from_store(&store).unwrap().len(), 2);
    }
}

#[test]
fn stale_playlist_receipts_cannot_publish_or_consume_a_new_essential_read() {
    let mut store = MemStore::default();
    let (old, _) = store.adopt_spotify_library_owner(OWNER).unwrap();
    store
        .adopt_spotify_library_owner("another-test-owner")
        .unwrap();
    let (current, _) = store.adopt_spotify_library_owner(OWNER).unwrap();
    apply_library_read(&mut store, &[song(9)]).unwrap();
    assert!(
        store
            .save_spotify_library_progress(OWNER, Some(&current), None, "new-read")
            .unwrap()
    );
    let before = store.library().unwrap();
    assert!(
        publish_library_read(&mut store, &old, &[song(1)], &Taste::default())
            .unwrap()
            .is_none()
    );
    assert_eq!(store.library().unwrap(), before);
    assert_eq!(
        store
            .spotify_library_progress(OWNER, Some(&current))
            .unwrap()
            .as_deref(),
        Some("new-read")
    );
}

#[test]
fn essential_publication_still_requires_the_exact_current_read() {
    let mut store = MemStore::default();
    let (owner, _) = store.adopt_spotify_library_owner(OWNER).unwrap();
    apply_library_read(&mut store, &[song(1)]).unwrap();
    assert!(
        store
            .save_spotify_library_progress(OWNER, Some(&owner), None, "current-read")
            .unwrap()
    );
    let before = store.library().unwrap();
    assert!(
        publish_saved_library(
            &mut store,
            &owner,
            "stale-read",
            &[song(2)],
            &Taste::default()
        )
        .unwrap()
        .is_none()
    );
    assert_eq!(store.library().unwrap(), before);
    assert!(
        publish_saved_library(
            &mut store,
            &owner,
            "current-read",
            &[song(2)],
            &Taste::default()
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
    assert!(pool.get(&song(2).source_uri).is_some());
}
