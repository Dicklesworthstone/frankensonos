use super::*;
use serde_json::json;

fn request() -> QueueSelectionRequest {
    serde_json::from_value(json!({
        "zone": "Den@S2", "target": "selected",
        "positions": [3, 1, 3], "expected_revision": "source-queue",
        "mode": "next", "expected_queue_revision": "destination-queue"
    }))
    .unwrap()
}

#[test]
fn source_and_destination_revisions_preserve_the_reviewed_occurrences() {
    let request = request();
    request.validate().unwrap();
    assert_eq!(request.positions, [3, 1, 3]);
    assert_eq!(request.expected_revision, "source-queue");
    assert_eq!(
        request.expected_queue_revision.as_deref(),
        Some("destination-queue")
    );
    let input = request.input();
    assert_eq!(input.target, "selected");
    assert!(input.playlist.is_none());
    assert_eq!(input.positions, [3, 1, 3]);
}

#[test]
fn invalid_and_client_supplied_renderer_selections_are_rejected() {
    let valid = serde_json::to_value(request()).unwrap();
    for field in ["positions", "expected_revision", "mode", "zone"] {
        let mut missing = valid.clone();
        missing.as_object_mut().unwrap().remove(field);
        assert!(serde_json::from_value::<QueueSelectionRequest>(missing).is_err());
    }
    for field in ["uri", "metadata", "tracks", "playlist"] {
        let mut extra = valid.clone();
        extra[field] = json!("client-controlled");
        assert!(serde_json::from_value::<QueueSelectionRequest>(extra).is_err());
    }
    for positions in [vec![], vec![1; 11], vec![0], vec![u32::MAX]] {
        assert!(
            QueueSelectionRequest {
                positions,
                ..request()
            }
            .validate()
            .is_err()
        );
    }
    for revision in ["".into(), " \n".into(), "x".repeat(1025)] {
        assert!(
            QueueSelectionRequest {
                expected_revision: revision,
                ..request()
            }
            .validate()
            .is_err()
        );
    }
}
