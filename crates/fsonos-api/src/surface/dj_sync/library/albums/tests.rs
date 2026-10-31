use super::super::{AlbumObservation, BrowsePage, BrowseQuery};
use crate::Surface;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::{AlbumTrack, LibraryEntry, MemStore, SqliteStore, Store};
use fsonos_proto::{ProtoError, Transport};
use std::net::IpAddr;

const ALBUM: &str = "spotify:album:Recording";

struct NoLan;
impl Transport for NoLan {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("album browsing must not contact speakers")
    }
}

fn surface(store: Box<dyn Store + Send>) -> Surface {
    Surface::new(
        Box::new(NoLan),
        Box::new(|_| panic!("album browsing must not discover speakers")),
        Policy::default(),
        Box::new(SystemClock),
    )
    .with_action_log(store, "album-test")
}

fn liked() -> LibraryEntry {
    let mut entry = LibraryEntry::default();
    entry.track.source_uri = "spotify:track:Liked".into();
    entry.track.title = "Old library title".into();
    entry.track.artist = Some("Known performer".into());
    entry.track.album = Some("Four movements".into());
    entry.album_uri = Some(ALBUM.into());
    // The complete observation supplies positions and durations.
    entry.disc_number = Some(9);
    entry.track_number = Some(9);
    entry.track.duration_secs = Some(999);
    entry.explicit = true;
    entry.candidate = Some(false);
    entry
}

fn recording() -> Vec<AlbumTrack> {
    [
        (2, 1, "Repeated", "Finale"),
        (1, 3, "Liked", "Adagio"),
        (1, 2, "Middle", "Scherzo"),
        (1, 1, "Repeated", "Prelude"),
    ]
    .into_iter()
    .map(|(disc_number, track_number, id, title)| AlbumTrack {
        disc_number,
        track_number,
        source_uri: format!("spotify:track:{id}"),
        title: title.into(),
        duration_secs: Some(180),
    })
    .collect()
}

fn query() -> BrowseQuery {
    BrowseQuery {
        album_uri: Some(ALBUM.into()),
        ..BrowseQuery::default()
    }
}

fn read(surface: &Surface, query: &BrowseQuery) -> BrowsePage {
    let page = surface
        .browse_library(&Client::LoopbackHttp, query)
        .unwrap();
    page.validate_for(query, None).unwrap();
    page
}

fn exercise(store: Box<dyn Store + Send>) {
    let surface = surface(store);
    let log = surface.log.as_ref().unwrap();
    log.store
        .lock()
        .unwrap()
        .upsert_library(&[liked()])
        .unwrap();
    let subset = read(&surface, &query());
    assert_eq!(subset.total, 1);
    assert_eq!(
        subset.album_observation,
        Some(AlbumObservation {
            coverage: "library_subset".into(),
            fetched_at: None,
            cached_total: 1,
        })
    );
    assert!(subset.text().contains("Library subset"));
    log.store
        .lock()
        .unwrap()
        .save_album_tracks(ALBUM, &recording(), 100)
        .unwrap();

    let complete = read(&surface, &query());
    assert_eq!(
        complete
            .items
            .iter()
            .map(|item| item.title.as_str())
            .collect::<Vec<_>>(),
        ["Prelude", "Scherzo", "Adagio", "Finale"]
    );
    assert_eq!(complete.items[0].source_uri, complete.items[3].source_uri);
    assert_eq!(
        complete.album_observation,
        Some(AlbumObservation {
            coverage: "complete".into(),
            fetched_at: Some(100),
            cached_total: 4,
        })
    );
    assert_ne!(complete.revision, subset.revision);
    let missing = &complete.items[0];
    assert!(
        missing.subtitle.is_empty(),
        "do not borrow another movement's performer"
    );
    assert!(!missing.explicit_known);
    assert!(!missing.explicit);
    let known = &complete.items[2];
    assert_eq!(known.subtitle, "Known performer");
    assert!(known.explicit_known && known.explicit);
    assert_eq!(
        (known.disc_number, known.track_number, known.duration_secs),
        (Some(1), Some(3), Some(180))
    );
    let text = complete.text();
    assert!(text.contains("Complete cached album: 4"));
    assert!(text.contains("1970-01-01 00:01:40 UTC"));
    assert!(text.contains("Artist unavailable"));
    assert!(text.contains("Explicit-content metadata unavailable"));
    assert_eq!(
        text.matches("source_uri: spotify:track:Repeated").count(),
        2
    );

    let filtered = read(
        &surface,
        &BrowseQuery {
            q: Some("four scherzo".into()),
            ..query()
        },
    );
    assert_eq!(filtered.total, 1);
    assert_eq!(filtered.items[0].source_uri, "spotify:track:Middle");
    assert_eq!(filtered.album_observation.as_ref().unwrap().cached_total, 4);
    let first_query = BrowseQuery {
        limit: Some(2),
        ..query()
    };
    let first = read(&surface, &first_query);
    let second_query = BrowseQuery {
        offset: first.next_offset,
        ..first_query.clone()
    };
    let second = read(&surface, &second_query);
    second
        .validate_for(&second_query, Some(&first.revision))
        .unwrap();
    assert_eq!(second.items[1].track_number, Some(1));
    assert_eq!(second.items[1].disc_number, Some(2));

    let global = read(&surface, &BrowseQuery::default());
    assert_eq!(
        global.total, 1,
        "catalog tracks must not enter global library browsing"
    );
    assert!(global.album_observation.is_none());
    let stored = log.store.lock().unwrap().library().unwrap();
    assert_eq!(
        stored,
        vec![liked()],
        "browsing cannot promote or rewrite personal tracks"
    );

    log.store
        .lock()
        .unwrap()
        .save_album_tracks(ALBUM, &recording(), 101)
        .unwrap();
    let refreshed = read(&surface, &query());
    assert_ne!(
        complete.revision, refreshed.revision,
        "fetch time is observation identity"
    );
    assert!(
        refreshed
            .validate_for(&query(), Some(&complete.revision))
            .is_err()
    );
    let narrow = BrowseQuery {
        q: Some("Prelude".into()),
        ..query()
    };
    let before_hidden_change = read(&surface, &narrow);
    let mut changed = recording();
    changed[0].title = "New finale".into();
    log.store
        .lock()
        .unwrap()
        .save_album_tracks(ALBUM, &changed, 101)
        .unwrap();
    let replaced = read(&surface, &query());
    assert_ne!(
        refreshed.revision, replaced.revision,
        "content replacement changes identity"
    );
    let after_hidden_change = read(&surface, &narrow);
    assert_eq!(before_hidden_change.items, after_hidden_change.items);
    assert_ne!(
        before_hidden_change.revision, after_hidden_change.revision,
        "a changed hidden occurrence still replaces the album observation"
    );
}

#[test]
fn completed_albums_are_visible_without_promoting_library_rows_in_memory() {
    exercise(Box::new(MemStore::default()));
}

#[test]
fn completed_albums_are_visible_through_the_real_sqlite_store() {
    exercise(Box::new(SqliteStore::open_in_memory().unwrap()));
}

#[test]
fn old_daemon_pages_keep_known_explicitness_and_new_observations_validate() {
    let surface = surface(Box::new(MemStore::default()));
    surface
        .log
        .as_ref()
        .unwrap()
        .store
        .lock()
        .unwrap()
        .upsert_library(&[liked()])
        .unwrap();
    let mut legacy = serde_json::to_value(read(&surface, &query())).unwrap();
    legacy.as_object_mut().unwrap().remove("album_observation");
    legacy["items"][0]
        .as_object_mut()
        .unwrap()
        .remove("explicit_known");
    let page: BrowsePage = serde_json::from_value(legacy).unwrap();
    page.validate_for(&query(), None).unwrap();
    assert!(page.items[0].explicit_known);

    surface
        .log
        .as_ref()
        .unwrap()
        .store
        .lock()
        .unwrap()
        .save_album_tracks(ALBUM, &recording(), 100)
        .unwrap();
    let complete = read(&surface, &query());
    let mutations: [fn(&mut BrowsePage); 4] = [
        |page: &mut BrowsePage| {
            page.album_observation.as_mut().unwrap().cached_total = 0;
        },
        |page: &mut BrowsePage| {
            page.album_observation.as_mut().unwrap().fetched_at = None;
        },
        |page: &mut BrowsePage| {
            page.items[1].track_number = page.items[0].track_number;
        },
        |page: &mut BrowsePage| {
            page.items[0].explicit = true;
        },
    ];
    for mutate in mutations {
        let mut invalid = complete.clone();
        mutate(&mut invalid);
        assert!(invalid.validate_for(&query(), None).is_err());
    }
    let json = serde_json::to_value(&complete).unwrap();
    assert_eq!(json["items"][0]["explicit_known"], false);
    assert!(json["items"][2].get("explicit_known").is_none());
}
