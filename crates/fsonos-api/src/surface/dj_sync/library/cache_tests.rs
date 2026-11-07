//! Surface integration: warm indexes observe real store writes and store replacement.

use super::*;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::Policy;
use fsonos_core::store::{AlbumTrack, MemStore, Store};
use fsonos_proto::{ProtoError, Transport};
use std::net::IpAddr;

struct NoLan;

impl Transport for NoLan {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("cached catalog browsing must not contact a speaker")
    }
}

fn surface(store: MemStore) -> Surface {
    Surface::new(
        Box::new(NoLan),
        Box::new(|_| panic!("cached catalog browsing must not survey the LAN")),
        Policy::default(),
        Box::new(SystemClock),
    )
    .with_action_log(Box::new(store), "library-cache-test")
}

fn track(title: &str) -> LibraryEntry {
    let mut entry = LibraryEntry::default();
    entry.track.source_uri = "spotify:track:T".into();
    entry.track.title = title.into();
    entry.track.artist = Some("Performer".into());
    entry.track.album = Some("Recording".into());
    entry.album_uri = Some("spotify:album:A".into());
    entry.disc_number = Some(1);
    entry.track_number = Some(1);
    entry
}

fn read(surface: &Surface, query: &BrowseQuery) -> BrowsePage {
    surface
        .browse_library(&Client::LoopbackHttp, query)
        .unwrap()
}

fn change(surface: &Surface, update: impl FnOnce(&mut dyn Store)) {
    let mut store = surface.log.as_ref().unwrap().store.lock().unwrap();
    update(store.as_mut());
}

fn playlists() -> BrowseQuery {
    BrowseQuery {
        kind: Some("playlists".into()),
        ..BrowseQuery::default()
    }
}

fn album() -> BrowseQuery {
    BrowseQuery {
        album_uri: Some("spotify:album:A".into()),
        ..BrowseQuery::default()
    }
}

#[test]
fn warm_track_album_and_playlist_views_observe_public_catalog_writes() {
    let mut store = MemStore::default();
    store.upsert_library(&[track("Before")]).unwrap();
    store
        .save_playlists(&[("spotify:playlist:P".into(), "Old name".into())])
        .unwrap();
    let surface = surface(store);
    let first = read(&surface, &BrowseQuery::default());
    let first_album = read(&surface, &album());
    assert_eq!(read(&surface, &playlists()).items[0].title, "Old name");

    change(&surface, |store| {
        let mut entry = track("After");
        entry.track.album = Some("New recording title".into());
        entry.explicit = true;
        store.upsert_library(&[entry]).unwrap();
        store
            .save_playlists(&[("spotify:playlist:P".into(), "New name".into())])
            .unwrap();
    });

    let next = read(&surface, &BrowseQuery::default());
    assert_eq!(next.items[0].title, "After");
    assert!(next.items[0].explicit);
    assert_ne!(next.revision, first.revision);
    let next_album = read(&surface, &album());
    assert_eq!(
        next_album.items[0].album.as_deref(),
        Some("New recording title")
    );
    assert_ne!(next_album.revision, first_album.revision);
    assert_eq!(read(&surface, &playlists()).items[0].title, "New name");
}

#[test]
fn completed_album_replacement_and_deletion_invalidate_a_warm_drill_down() {
    let mut store = MemStore::default();
    store.upsert_library(&[track("Saved")]).unwrap();
    let surface = surface(store);
    let subset = read(&surface, &album());
    assert_eq!(
        subset.album_observation.as_ref().unwrap().coverage,
        "library_subset"
    );

    change(&surface, |store| {
        store
            .save_album_tracks(
                "spotify:album:A",
                &[AlbumTrack {
                    source_uri: "spotify:track:Catalog".into(),
                    title: "Catalog-only".into(),
                    disc_number: 1,
                    track_number: 1,
                    duration_secs: Some(60),
                    explicit: None,
                }],
                10,
            )
            .unwrap();
    });
    let complete = read(&surface, &album());
    assert_eq!(complete.items[0].title, "Catalog-only");
    assert!(!complete.items[0].explicit_known);
    assert_eq!(
        complete.album_observation.as_ref().unwrap().fetched_at,
        Some(10)
    );

    change(&surface, |store| {
        store.save_album_tracks("spotify:album:A", &[], 20).unwrap();
    });
    let removed = read(&surface, &album());
    assert_eq!(removed, subset);
    assert_ne!(removed.revision, complete.revision);
}

#[test]
fn an_owner_switch_clears_cached_playlists_and_refuses_old_publications() {
    let mut store = MemStore::default();
    let (old_owner, _) = store.adopt_spotify_library_owner("owner-a").unwrap();
    store.upsert_library(&[track("Saved")]).unwrap();
    store
        .save_playlists(&[("spotify:playlist:P".into(), "Old owner".into())])
        .unwrap();
    let surface = surface(store);
    assert_eq!(read(&surface, &playlists()).total, 1);

    change(&surface, |store| {
        store.adopt_spotify_library_owner("owner-b").unwrap();
    });
    assert_eq!(read(&surface, &playlists()).total, 0);
    let retired = read(&surface, &BrowseQuery::default());
    // Previously cached tracks remain browsable; adoption retires DJ eligibility.
    assert_eq!(retired.items[0].title, "Saved");

    change(&surface, |store| {
        let before = store.library_catalog_revision().unwrap();
        assert!(
            !store
                .publish_spotify_library(
                    &old_owner,
                    &[track("Stale")],
                    Some(&[("spotify:playlist:P".into(), "Stale owner".into())]),
                    &[],
                    None,
                )
                .unwrap()
        );
        assert_eq!(store.library_catalog_revision().unwrap(), before);
    });
    assert_eq!(read(&surface, &playlists()).total, 0);
    assert_eq!(read(&surface, &BrowseQuery::default()), retired);
}

#[test]
fn accepted_publication_replaces_warm_results_without_changing_the_wire_contract() {
    let mut store = MemStore::default();
    let (owner, _) = store.adopt_spotify_library_owner("owner").unwrap();
    store.upsert_library(&[track("Before")]).unwrap();
    let surface = surface(store);
    read(&surface, &BrowseQuery::default());
    change(&surface, |store| {
        assert!(
            store
                .publish_spotify_library(
                    &owner,
                    &[track("Published")],
                    Some(&[("spotify:playlist:P".into(), "Published list".into())]),
                    &[],
                    None,
                )
                .unwrap()
        );
    });
    let page = read(&surface, &BrowseQuery::default());
    assert_eq!(page.items[0].title, "Published");
    assert_eq!(
        serde_json::to_value(&page).unwrap(),
        serde_json::to_value(browse(&[track("Published")], &[], &BrowseQuery::default()).unwrap())
            .unwrap()
    );
    assert_eq!(
        read(&surface, &playlists()).items[0].title,
        "Published list"
    );
}

#[test]
fn replacing_the_store_cannot_reuse_an_equal_counter_from_a_different_catalog() {
    let mut first = MemStore::default();
    first.upsert_library(&[track("First store")]).unwrap();
    let mut second = MemStore::default();
    second.upsert_library(&[track("Second store")]).unwrap();
    assert_eq!(
        first.library_catalog_revision().unwrap(),
        second.library_catalog_revision().unwrap()
    );
    let surface = surface(first);
    assert_eq!(
        read(&surface, &BrowseQuery::default()).items[0].title,
        "First store"
    );
    let surface = surface.with_action_log(Box::new(second), "replacement");
    assert_eq!(
        read(&surface, &BrowseQuery::default()).items[0].title,
        "Second store"
    );
}
