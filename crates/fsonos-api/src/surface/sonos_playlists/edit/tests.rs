use super::*;
use serde_json::json;

fn removal() -> RemoveSonosPlaylistTracksRequest {
    RemoveSonosPlaylistTracksRequest {
        zone: "Den@S2".into(),
        playlist: "SQ:7".into(),
        expected_revision: "opaque contents revision".into(),
        position: 2,
        count: 3,
    }
}

#[test]
fn occurrence_edits_require_exact_saved_identity_revision_and_bounded_ranges() {
    removal().validate().unwrap();
    for playlist in ["Evening", "7", "SQ:", "SQ:7/3", "FV:2/7", "SQ:-1"] {
        assert!(
            RemoveSonosPlaylistTracksRequest {
                playlist: playlist.into(),
                ..removal()
            }
            .validate()
            .is_err()
        );
    }
    for revision in ["", "  ", "a\nb"] {
        assert!(
            RemoveSonosPlaylistTracksRequest {
                expected_revision: revision.into(),
                ..removal()
            }
            .validate()
            .is_err()
        );
    }
    for (position, count) in [
        (0, 1),
        (1, 0),
        (1, playlists::MAX_PLAYLIST_EDIT_TRACKS + 1),
        (u32::MAX, 2),
    ] {
        assert!(
            RemoveSonosPlaylistTracksRequest {
                position,
                count,
                ..removal()
            }
            .validate()
            .is_err()
        );
    }
    RemoveSonosPlaylistTracksRequest {
        position: u32::MAX,
        count: 1,
        ..removal()
    }
    .validate()
    .unwrap();
}

#[test]
fn move_requests_keep_original_positions_and_reject_implicit_defaults() {
    let request: MoveSonosPlaylistTracksRequest = serde_json::from_value(json!({
        "zone":"Den@S2", "playlist":"SQ:7", "expected_revision":"r",
        "position":2, "count":2, "insert_before":7,
    }))
    .unwrap();
    request.validate().unwrap();
    assert_eq!(request.insert_before, 7);
    assert!(
        MoveSonosPlaylistTracksRequest {
            insert_before: 0,
            ..request
        }
        .validate()
        .is_err()
    );
    let body =
        json!({"zone":"Den", "playlist":"SQ:7", "expected_revision":"r", "position":2, "count":2});
    for field in ["zone", "playlist", "expected_revision", "position", "count"] {
        let mut missing = body.clone();
        missing.as_object_mut().unwrap().remove(field);
        assert!(serde_json::from_value::<RemoveSonosPlaylistTracksRequest>(missing).is_err());
    }
    assert!(serde_json::from_value::<MoveSonosPlaylistTracksRequest>(body.clone()).is_err());
    for field in [
        "force",
        "uri",
        "metadata",
        "insert_before",
        "expected_queue_revision",
    ] {
        let mut extra = body.clone();
        extra[field] = json!("untrusted");
        assert!(serde_json::from_value::<RemoveSonosPlaylistTracksRequest>(extra).is_err());
    }
}

#[test]
fn logging_failure_keeps_the_confirmed_playlist_revision_visible() {
    let result = EditedSonosPlaylistDto {
        done: "removed entry from saved playlist".into(),
        zone: "Den".into(),
        playlist: SonosPlaylistDto {
            id: "SQ:7".into(),
            title: "Evening".into(),
            art_uri: None,
        },
        operation: SonosPlaylistEditOperation::RemoveTracks,
        position: 2,
        count: 1,
        insert_before: None,
        previous_total: 3,
        total: 2,
        revision: "confirmed:new-revision".into(),
        changed: true,
        logged: false,
    };
    assert!(result.text().contains("Revision: confirmed:new-revision"));
    assert!(result.text().contains("do not repeat this operation"));
    assert!(
        serde_json::to_value(&result)
            .unwrap()
            .get("insert_before")
            .is_none()
    );
    let logged = EditedSonosPlaylistDto {
        logged: true,
        ..result
    };
    assert!(!logged.text().contains("could not be written"));
}
