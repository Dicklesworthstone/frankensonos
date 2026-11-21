use super::*;
use serde_json::json;

fn request() -> AppendSonosPlaylistTracksRequest {
    AppendSonosPlaylistTracksRequest {
        zone: "Den@S2".into(),
        playlist: "SQ:7".into(),
        expected_revision: "destination:observed".into(),
        source_playlist: None,
        source_revision: "queue:observed".into(),
        positions: vec![2, 4, 2],
    }
}

#[test]
fn sources_require_independent_proofs_and_preserve_ordered_occurrences() {
    let queue = request();
    queue.validate().unwrap();
    assert_eq!(queue.positions, [2, 4, 2]);
    let saved = AppendSonosPlaylistTracksRequest {
        source_playlist: Some("SQ:7".into()),
        source_revision: "saved:observed".into(),
        ..queue
    };
    saved.validate().unwrap();
    assert_eq!(saved.positions, [2, 4, 2]);
    assert_ne!(saved.source_revision, saved.expected_revision);
    for id in ["Evening", "7", "SQ:", "SQ:7/3", "FV:2/7", "SQ:-1"] {
        assert!(
            AppendSonosPlaylistTracksRequest {
                playlist: id.into(),
                ..request()
            }
            .validate()
            .is_err()
        );
        assert!(
            AppendSonosPlaylistTracksRequest {
                source_playlist: Some(id.into()),
                ..request()
            }
            .validate()
            .is_err()
        );
    }
    for revision in ["", "  ", "a\nb"] {
        assert!(
            AppendSonosPlaylistTracksRequest {
                source_revision: revision.into(),
                ..request()
            }
            .validate()
            .is_err()
        );
        assert!(
            AppendSonosPlaylistTracksRequest {
                expected_revision: revision.into(),
                ..request()
            }
            .validate()
            .is_err()
        );
    }
    for positions in [vec![], vec![0], vec![1; 11], vec![u32::MAX]] {
        assert!(
            AppendSonosPlaylistTracksRequest {
                positions,
                ..request()
            }
            .validate()
            .is_err()
        );
    }
}

#[test]
fn request_cannot_replace_speaker_metadata_or_skip_a_revision() {
    let body = json!({
        "zone": "Den@S2", "playlist": "SQ:7", "expected_revision": "destination",
        "source_revision": "queue", "positions": [2, 4, 2],
    });
    let parsed: AppendSonosPlaylistTracksRequest = serde_json::from_value(body.clone()).unwrap();
    assert!(parsed.source_playlist.is_none());
    assert!(
        serde_json::to_value(parsed)
            .unwrap()
            .get("source_playlist")
            .is_none()
    );
    for field in [
        "zone",
        "playlist",
        "expected_revision",
        "source_revision",
        "positions",
    ] {
        let mut missing = body.clone();
        missing.as_object_mut().unwrap().remove(field);
        assert!(serde_json::from_value::<AppendSonosPlaylistTracksRequest>(missing).is_err());
    }
    for field in ["uri", "metadata", "force", "host", "next"] {
        let mut extra = body.clone();
        extra[field] = json!("untrusted");
        assert!(serde_json::from_value::<AppendSonosPlaylistTracksRequest>(extra).is_err());
    }
}

#[test]
fn confirmed_revision_survives_an_action_log_failure() {
    let result = AppendedSonosPlaylistDto {
        done: "added 3 tracks to Evening".into(),
        zone: "Den".into(),
        playlist: SonosPlaylistDto {
            id: "SQ:7".into(),
            title: "Evening".into(),
            art_uri: None,
        },
        revision: "confirmed:new-revision".into(),
        previous_total: 4,
        total: 7,
        added: 3,
        positions: vec![2, 4, 2],
        titles: vec!["Second".into(), "Fourth".into(), "Second".into()],
        logged: false,
    };
    assert!(result.text().contains("Revision: confirmed:new-revision"));
    assert!(result.text().contains("do not repeat this operation"));
    assert!(
        !AppendedSonosPlaylistDto {
            logged: true,
            ..result
        }
        .text()
        .contains("could not be written")
    );
}
