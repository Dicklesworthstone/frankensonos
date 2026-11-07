//! Durable intake uses the existing validated projection, not raw API JSON.

use super::*;
use serde_json::{Value, json};

const OWNER: &str = "test-owner";
const NOW: i64 = 1_000_000;

fn bytes(value: &Value) -> Vec<u8> {
    serde_json::to_vec(value).unwrap()
}

fn track(position: u32) -> Value {
    json!({
        "uri": format!("spotify:track:Progress{position}"),
        "name": format!("Song {position}"),
        "artists": [{ "id": "ProgressArtist", "name": "Test Artist" }],
        "album": {
            "name": "Test Album", "uri": "spotify:album:ProgressAlbum",
            "artists": [{ "name": "Test Album Artist" }], "release_date": "1982"
        },
        "duration_ms": 180_000, "disc_number": 1, "track_number": position + 1,
        "is_playable": true, "explicit": position == 1
    })
}

fn liked(position: u32, total: u32) -> Value {
    json!({
        "items": [{ "added_at": "2025-01-15T20:31:02Z", "track": track(position) }],
        "offset": position, "total": total, "next": null
    })
}

fn empty() -> Value {
    json!({ "items": [], "offset": 0, "total": 0, "next": null })
}

fn fresh() -> LibraryProgress {
    let mut progress = LibraryProgress::new(OWNER, &Endpoints::default(), NOW);
    progress.nonce = "test-progress-nonce".into();
    progress
}

fn prefix() -> LibraryProgress {
    let mut progress = fresh();
    progress.ingest(&bytes(&empty())).unwrap();
    progress.ingest(&bytes(&liked(0, 3))).unwrap();
    progress
}

fn restored(progress: &LibraryProgress) -> LibraryProgress {
    LibraryProgress::restore(
        &progress.encode().unwrap(),
        OWNER,
        &Endpoints::default(),
        NOW + 1,
    )
    .unwrap()
}

#[test]
fn a_checkpoint_resumes_the_next_offset_with_complete_track_metadata() {
    let before = prefix();
    let mut resumed = restored(&before);
    resumed.claim().unwrap();
    assert_eq!(resumed.pages(), 2);
    assert_eq!(resumed.tracks(), 1);
    assert_eq!(resumed.read.items, before.read.items);
    let next = Endpoints::default().saved_tracks(1);
    assert_eq!(resumed.next_url(), Some(next.as_str()));
    assert!(resumed.verification_required);
    resumed.ingest(&bytes(&liked(1, 3))).unwrap();
    let again = restored(&resumed);
    let item = &again.read.items[1];
    assert_eq!(item.artist_id.as_deref(), Some("ProgressArtist"));
    assert_eq!(
        item.album_uri.as_deref(),
        Some("spotify:album:ProgressAlbum")
    );
    assert_eq!((item.disc_number, item.track_number), (Some(1), Some(2)));
    assert_eq!(item.added_at, Some(1_736_973_062));
    assert_eq!(item.release_year, Some(1982));
    assert!(item.explicit);
    assert_eq!(item.origin, Origin::LikedTrack);
}

#[test]
fn proof_ignores_unused_fields_and_query_spelling_but_not_music_metadata() {
    let progress = prefix();
    let mut equivalent = liked(0, 3);
    equivalent["items"][0]["track"]["popularity"] = json!(42);
    equivalent["items"][0]["track"]["images"] = json!([{ "url": "ignored" }]);
    equivalent["next"] = json!("https://api.spotify.com/v1/me/tracks?limit=1&%6Fffset=1");
    assert!(progress.matches(1, &bytes(&equivalent)).unwrap());
    // Reformatting or object key order has no bearing on accepted evidence.
    let pretty = serde_json::to_vec_pretty(&equivalent).unwrap();
    assert!(progress.matches(1, &pretty).unwrap());
    for (field, changed) in [
        ("name", json!("Replacement song")),
        ("explicit", json!(true)),
        ("duration_ms", json!(210_000)),
    ] {
        let mut replacement = equivalent.clone();
        replacement["items"][0]["track"][field] = changed;
        assert!(
            !progress.matches(1, &bytes(&replacement)).unwrap(),
            "{field}"
        );
    }
}

#[test]
fn unchanged_roots_do_not_hide_a_same_total_middle_replacement() {
    let mut progress = prefix();
    progress.ingest(&bytes(&liked(1, 3))).unwrap();
    progress.ingest(&bytes(&liked(2, 3))).unwrap();
    progress.claim().unwrap();
    assert!(progress.matches(0, &bytes(&empty())).unwrap());
    assert!(progress.matches(1, &bytes(&liked(0, 3))).unwrap());
    let mut middle = liked(1, 3);
    middle["items"][0]["track"] = track(99);
    assert!(!progress.matches(2, &bytes(&middle)).unwrap());
    assert_eq!(progress.next_check(), Some(0));
    assert!(!progress.finish());
}

#[test]
fn verification_cursor_survives_a_restart_and_ready_proofs_are_rechecked() {
    let mut progress = prefix();
    progress.ingest(&bytes(&liked(1, 3))).unwrap();
    progress.ingest(&bytes(&liked(2, 3))).unwrap();
    assert!(progress.finish(), "one uninterrupted read needs no replay");
    let mut resumed = restored(&progress);
    let previous_nonce = resumed.nonce.clone();
    resumed.claim().unwrap();
    assert_ne!(resumed.nonce, previous_nonce);
    assert_eq!(resumed.next_check(), Some(0));
    assert!(!resumed.finish());
    resumed.checked();
    let mut later = restored(&resumed);
    later.claim().unwrap();
    assert_eq!(
        later.next_check(),
        Some(1),
        "verification makes durable progress"
    );
    for index in 1..4 {
        assert_eq!(later.next_check(), Some(index));
        later.checked();
    }
    assert!(later.finish());
    assert!(restored(&later).ready);
    later.claim().unwrap();
    assert_eq!(
        later.next_check(),
        Some(0),
        "unpublished completed reads age too"
    );
}

#[test]
fn owner_endpoint_version_and_age_are_part_of_checkpoint_identity() {
    let encoded = prefix().encode().unwrap();
    let endpoints = Endpoints::default();
    assert!(LibraryProgress::restore(&encoded, "other-owner", &endpoints, NOW).is_none());
    let other = Endpoints {
        api: "https://example.invalid/v1".into(),
        ..endpoints.clone()
    };
    assert!(LibraryProgress::restore(&encoded, OWNER, &other, NOW).is_none());
    assert!(LibraryProgress::restore(&encoded, OWNER, &endpoints, NOW + MAX_AGE_SECS).is_none());
    assert!(LibraryProgress::restore(&encoded, OWNER, &endpoints, NOW - 301).is_none());
    let mut value: Value = serde_json::from_str(&encoded).unwrap();
    value["version"] = json!(VERSION + 1);
    assert!(LibraryProgress::restore(&value.to_string(), OWNER, &endpoints, NOW).is_none());
}

#[test]
fn corrupt_retained_items_are_rejected_before_any_request_can_use_them() {
    let snapshot = serde_json::to_value(prefix()).unwrap();
    for field in ["title", "explicit", "artist_id", "origin"] {
        let mut changed = snapshot.clone();
        changed["read"]["items"][0][field] = match field {
            "explicit" => json!(true),
            "origin" => json!("Taste"),
            _ => json!("changed"),
        };
        assert!(
            LibraryProgress::restore(&changed.to_string(), OWNER, &Endpoints::default(), NOW)
                .is_none(),
            "{field}"
        );
    }
    let mut missing = snapshot;
    missing["read"]["items"][0]
        .as_object_mut()
        .unwrap()
        .remove("explicit");
    assert!(
        LibraryProgress::restore(&missing.to_string(), OWNER, &Endpoints::default(), NOW).is_none()
    );
}

#[test]
fn outstanding_fetches_cannot_be_redirected_skipped_or_invented() {
    let snapshot = serde_json::to_value(prefix()).unwrap();
    for url in [
        "https://example.invalid/v1/me/tracks?offset=1",
        "https://api.spotify.com/v1/me/albums?offset=1",
        "https://api.spotify.com/v1/me/tracks?offset=2",
        "https://api.spotify.com/v1/me/tracks?offset=1&offset=2",
    ] {
        let mut changed = snapshot.clone();
        changed["read"]["pending"][0]["SavedTracks"][0] = json!(url);
        assert!(
            LibraryProgress::restore(&changed.to_string(), OWNER, &Endpoints::default(), NOW)
                .is_none(),
            "{url}"
        );
    }
    let mut missing = snapshot;
    missing["read"]["pending"] = json!([]);
    assert!(
        LibraryProgress::restore(&missing.to_string(), OWNER, &Endpoints::default(), NOW).is_none()
    );
}

#[test]
fn a_rejected_page_cannot_change_the_durable_accepted_prefix() {
    let mut progress = prefix();
    let before = progress.encode().unwrap();
    let mut missing_flag = liked(1, 3);
    missing_flag["items"][0]["track"]
        .as_object_mut()
        .unwrap()
        .remove("explicit");
    assert!(progress.ingest(&bytes(&missing_flag)).is_err());
    assert_eq!(progress.encode().unwrap(), before);
    let mut wrong_route = liked(1, 3);
    wrong_route["next"] = json!(Endpoints::default().saved_albums(2));
    assert!(progress.ingest(&bytes(&wrong_route)).is_err());
    assert_eq!(progress.encode().unwrap(), before);
    progress.ingest(&bytes(&liked(1, 3))).unwrap();
    assert_eq!(restored(&progress).tracks(), 2);
}

#[test]
fn saved_album_hydration_retains_save_metadata_and_checked_coverage() {
    let mut progress = fresh();
    let saved = json!({
        "items": [{ "added_at": "2025-01-15T20:31:02Z", "album": {
            "id": "ProgressAlbum", "uri": "spotify:album:ProgressAlbum",
            "name": "Saved Album", "artists": [{ "name": "Album Artist" }],
            "genres": ["chamber music"], "release_date": "1982", "label": "Test Label",
            "total_tracks": 2
        }}], "offset": 0, "total": 1, "next": null
    });
    progress.ingest(&bytes(&saved)).unwrap();
    progress.ingest(&bytes(&empty())).unwrap();
    let mut resumed = restored(&progress);
    resumed.claim().unwrap();
    let first = Endpoints::default().album_tracks("ProgressAlbum", 0);
    assert_eq!(resumed.next_url(), Some(first.as_str()));
    resumed
        .ingest(&bytes(&json!({
            "items": [track(0)], "offset": 0, "total": 2, "next": null
        })))
        .unwrap();
    let mut last = restored(&resumed);
    let next = Endpoints::default().album_tracks("ProgressAlbum", 1);
    assert_eq!(last.next_url(), Some(next.as_str()));
    last.ingest(&bytes(&json!({
        "items": [track(1)], "offset": 1, "total": 2, "next": null
    })))
    .unwrap();
    assert!(last.next_url().is_none());
    let item = &last.read.items[1];
    assert_eq!(item.album.as_deref(), Some("Saved Album"));
    assert_eq!(item.album_artists, ["Album Artist"]);
    assert_eq!(item.genres, ["chamber music"]);
    assert_eq!(item.label.as_deref(), Some("Test Label"));
    assert_eq!(item.added_at, Some(1_736_973_062));
    assert_eq!(item.origin, Origin::SavedAlbum);
    assert!(item.explicit);
    assert!(restored(&last).verification_required);
}

#[test]
fn changed_hydration_totals_restart_instead_of_wedging_the_old_continuation() {
    let mut progress = fresh();
    progress
        .ingest(&bytes(&json!({
            "items": [{ "album": {
                "id": "ProgressAlbum", "uri": "spotify:album:ProgressAlbum",
                "name": "Saved Album", "total_tracks": 2
            }}], "offset": 0, "total": 1, "next": null
        })))
        .unwrap();
    progress.ingest(&bytes(&empty())).unwrap();
    let changed = bytes(&json!({
        "items": [track(0)], "offset": 0, "total": 3, "next": null
    }));
    assert!(progress.collection_changed(&changed));
    assert!(!progress.collection_changed(b"{"));
    let original = bytes(&json!({
        "items": [track(0)], "offset": 0, "total": 2, "next": null
    }));
    progress.ingest(&original).unwrap();
    assert!(progress.matches(2, &original).unwrap());
    assert!(!progress.matches(2, &changed).unwrap());
    assert!(
        progress.matches(2, b"{").is_err(),
        "malformed data remains an error"
    );
}
