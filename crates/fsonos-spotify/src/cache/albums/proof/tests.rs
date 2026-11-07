//! Catalog drift, checkpoint integrity and bounded verification state.

use super::*;
use serde_json::{Value, json};

const NOW: i64 = 1_790_035_200;
const ALBUM: &str = "spotify:album:ProofAlbum";

fn track(number: u32) -> Value {
    json!({
        "uri": format!("spotify:track:Proof{number}"),
        "name": format!("Sonata: Movement {number}"),
        "disc_number": 1, "track_number": number,
        "duration_ms": 180_000, "explicit": false, "is_playable": true
    })
}

fn page(offset: u32, total: u32) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "items": [track(offset + 1)], "offset": offset, "total": total
    }))
    .unwrap()
}

fn read(count: u32, total: u32) -> PartialAlbum {
    let mut part = PartialAlbum::new(ALBUM.into(), NOW);
    for offset in 0..count {
        part.ingest(&page(offset, total), &Endpoints::default())
            .unwrap();
    }
    part
}

fn reopened(part: &PartialAlbum, at: i64) -> PartialAlbum {
    let mut loaded: PartialAlbum =
        serde_json::from_slice(&serde_json::to_vec(part).unwrap()).unwrap();
    assert!(loaded.resume(at));
    loaded
}

#[test]
fn uninterrupted_reads_complete_without_a_second_catalog_scan() {
    let part = read(3, 3);
    assert!(part.ready());
    assert_eq!(part.verification_index(), None);
    assert_eq!(part.album_tracks().len(), 3);
    assert!(part.valid(NOW));
}

#[test]
fn retained_pages_require_verification_before_becoming_complete() {
    let mut part = reopened(&read(2, 3), NOW + 10);
    let endpoints = Endpoints::default();
    assert!(!part.ready());
    assert!(part.verify(0, &page(0, 3), &endpoints).unwrap());
    part.ingest(&page(2, 3), &endpoints).unwrap();
    assert!(!part.ready());
    assert_eq!(part.verification_index(), Some(1));
    assert!(part.verify(1, &page(1, 3), &endpoints).unwrap());
    assert!(!part.ready());
    assert!(part.verify(2, &page(2, 3), &endpoints).unwrap());
    assert!(part.ready());
    assert_eq!(part.verified_positions(), 3);
}

#[test]
fn verification_keeps_its_cursor_across_another_restart() {
    let endpoints = Endpoints::default();
    let mut part = reopened(&read(10, 10), NOW + 10);
    for index in 0..6 {
        assert!(part.verify(index, &page(u32::try_from(index).unwrap(), 10), &endpoints).unwrap());
    }
    let mut part = reopened(&part, NOW + 20);
    assert_eq!(part.verification_index(), Some(6));
    assert!(part.verify(0, &page(0, 10), &endpoints).unwrap());
    assert_eq!(part.verification_index(), Some(6), "root checks do not reset progress");
    for index in 6..10 {
        assert!(part.verify(index, &page(u32::try_from(index).unwrap(), 10), &endpoints).unwrap());
    }
    assert!(part.ready());
    let again = reopened(&part, NOW + 30);
    assert_eq!(again.verification_index(), Some(0));
    assert!(!again.ready(), "a failed publication must recheck completed evidence");
}

#[test]
fn same_total_root_or_middle_metadata_changes_are_not_accepted() {
    for index in [0, 2] {
        for field in ["explicit", "name", "uri", "duration_ms", "track_number"] {
            let mut part = reopened(&read(4, 4), NOW + 1);
            let mut body: Value = serde_json::from_slice(&page(index, 4)).unwrap();
            body["items"][0][field] = match field {
                "explicit" => json!(true),
                "name" => json!("Another movement"),
                "uri" => json!("spotify:track:Replacement"),
                "duration_ms" => json!(300_000),
                _ => json!(99),
            };
            let before = part.clone();
            assert!(
                !part.verify(
                    index as usize,
                    &serde_json::to_vec(&body).unwrap(),
                    &Endpoints::default(),
                ).unwrap(),
                "accepted a changed {field} at page {index}"
            );
            assert_eq!(part, before, "rejected evidence does not advance verification");
        }
    }
}

#[test]
fn changed_total_is_detected_even_when_the_old_offset_no_longer_exists() {
    let mut part = reopened(&read(3, 3), NOW + 1);
    let body = serde_json::to_vec(&json!({
        "items": [], "offset": 2, "total": 1, "next": null
    })).unwrap();
    assert!(!part.verify(2, &body, &Endpoints::default()).unwrap());
    assert!(!part.ready());
}

#[test]
fn unused_json_metadata_and_key_order_do_not_restart_catalog_reads() {
    let mut part = reopened(&read(1, 1), NOW + 1);
    let body = br#"{
        "total":1,"href":"ignored","offset":0,"items":[{
            "explicit":false,"is_playable":true,"duration_ms":180000,
            "track_number":1,"disc_number":1,"name":"Sonata: Movement 1",
            "uri":"spotify:track:Proof1","popularity":98,
            "artists":[{"name":"Unused credit","id":"UnusedArtist"}]
        }],"next":null
    }"#;
    assert!(part.verify(0, body, &Endpoints::default()).unwrap());
    assert!(part.ready());
}

#[test]
fn raw_coverage_includes_unavailable_entries_and_detects_new_playability() {
    let endpoints = Endpoints::default();
    let mut unavailable = track(1);
    unavailable["is_playable"] = json!(false);
    let mut body = json!({
        "items": [unavailable, track(2)], "offset": 0, "total": 3
    });
    let mut part = PartialAlbum::new(ALBUM.into(), NOW);
    part.ingest(&serde_json::to_vec(&body).unwrap(), &endpoints).unwrap();
    assert_eq!(part.offset, 2);
    assert_eq!(part.album_tracks().len(), 1);
    let mut part = reopened(&part, NOW + 1);
    assert!(part.verify(0, &serde_json::to_vec(&body).unwrap(), &endpoints).unwrap());
    body["items"][0]["is_playable"] = json!(true);
    assert!(!part.verify(0, &serde_json::to_vec(&body).unwrap(), &endpoints).unwrap());
}

#[test]
fn omitted_later_totals_keep_the_original_coverage_proof() {
    let endpoints = Endpoints::default();
    let mut part = read(1, 2);
    let tail = serde_json::to_vec(&json!({
        "items": [track(2)], "offset": 1
    })).unwrap();
    part.ingest(&tail, &endpoints).unwrap();
    let mut part = reopened(&part, NOW + 1);
    assert!(part.verify(0, &page(0, 2), &endpoints).unwrap());
    assert!(part.verify(1, &tail, &endpoints).unwrap());
    assert!(part.ready());
}

#[test]
fn malformed_pages_never_advance_retained_metadata_or_verification() {
    let endpoints = Endpoints::default();
    let part = read(1, 2);
    for bad in [
        json!({ "items": [], "offset": 1, "total": 2 }),
        json!({ "items": [track(2)], "offset": 0, "total": 2 }),
        json!({ "items": [track(2)], "offset": 1, "total": 3 }),
        json!({
            "items": [track(2)], "offset": 1, "total": 2,
            "next": "https://example.invalid/track"
        }),
    ] {
        let mut attempt = part.clone();
        assert!(attempt.ingest(&serde_json::to_vec(&bad).unwrap(), &endpoints).is_err());
        assert_eq!(attempt, part);
    }
    let mut resumed = reopened(&part, NOW + 1);
    let before = resumed.clone();
    let missing_explicit = br#"{
        "items":[{"uri":"spotify:track:Proof1","name":"Sonata: Movement 1"}],
        "offset":0,"total":2
    }"#;
    assert!(resumed.verify(0, missing_explicit, &endpoints).is_err());
    assert_eq!(resumed, before);
}

#[test]
fn old_expired_future_or_corrupted_checkpoints_restart_instead_of_publishing() {
    let part = read(2, 3);
    assert!(part.clone().resume(NOW + MAX_AGE));
    assert!(!part.clone().resume(NOW + MAX_AGE + 1));
    assert!(!part.clone().resume(NOW - 1));
    let valid = serde_json::to_value(&part).unwrap();
    let mut changes = Vec::new();
    for field in ["version", "started_at", "proofs", "retained_bytes"] {
        let mut old = valid.clone();
        old.as_object_mut().unwrap().remove(field);
        changes.push(old);
    }
    let mut track_changed = valid.clone();
    track_changed["tracks"][0]["explicit"] = json!(true);
    changes.push(track_changed);
    let mut unknown_flag = valid.clone();
    unknown_flag["tracks"][0].as_object_mut().unwrap().remove("explicit");
    changes.push(unknown_flag);
    for (field, value) in [
        ("offset", json!(100)),
        ("complete", json!(true)),
        ("checked", json!(200)),
        ("pages", json!(200)),
        ("retained_bytes", json!(MAX_BYTES + 1)),
    ] {
        let mut changed = valid.clone();
        changed[field] = value;
        changes.push(changed);
    }
    let mut proof_changed = valid.clone();
    proof_changed["proofs"][1]["item_start"] = json!(0);
    changes.push(proof_changed);
    for changed in changes {
        let mut part: PartialAlbum = serde_json::from_value(changed).unwrap();
        assert!(!part.resume(NOW + 1));
    }
}

#[test]
fn oversize_pages_are_rejected_without_discarding_the_previous_prefix() {
    let mut part = read(1, 2);
    let before = part.clone();
    let mut huge = track(2);
    huge["name"] = json!("x".repeat(MAX_BYTES));
    let body = serde_json::to_vec(&json!({
        "items": [huge], "offset": 1, "total": 2
    })).unwrap();
    assert!(part.ingest(&body, &Endpoints::default()).is_err());
    assert_eq!(part, before);
}

#[test]
fn a_retained_empty_attempt_can_still_finish_in_one_fresh_pass() {
    let endpoints = Endpoints::default();
    let mut part = reopened(&PartialAlbum::new(ALBUM.into(), NOW), NOW + 1);
    assert!(!part.has_pages());
    part.ingest(&page(0, 1), &endpoints).unwrap();
    assert!(part.ready());
}
