use super::*;
use serde_json::json;

fn request() -> serde_json::Value {
    json!({
        "zone": "Den@S2", "playlist": "SQ:7", "positions": [3, 1, 3],
        "expected_revision": "sonos-playlist-v1-observed", "mode": "next",
        "expected_queue_revision": "q1:3:SIM:4:2",
    })
}

#[test]
fn native_selection_keeps_order_and_duplicate_occurrences_without_renderer_input() {
    let request: SonosPlaylistTracksRequest = serde_json::from_value(request()).unwrap();
    request.validate().unwrap();
    assert_eq!(request.positions, [3, 1, 3]);
    assert_eq!(request.mode, SonosPlaylistTrackMode::Next);
    assert_eq!(
        serde_json::to_value(&request).unwrap()["expected_queue_revision"],
        "q1:3:SIM:4:2"
    );
    for field in ["source_uri", "uri", "metadata", "title", "tracks", "next"] {
        let mut value = serde_json::to_value(&request).unwrap();
        value[field] = json!("caller-supplied");
        assert!(serde_json::from_value::<SonosPlaylistTracksRequest>(value).is_err());
    }
}

#[test]
fn both_observation_tokens_and_explicit_mode_have_distinct_contracts() {
    for mode in ["play", "next", "append"] {
        let mut value = request();
        value["mode"] = json!(mode);
        value
            .as_object_mut()
            .unwrap()
            .remove("expected_queue_revision");
        let asked: SonosPlaylistTracksRequest = serde_json::from_value(value).unwrap();
        asked.validate().unwrap();
        assert!(asked.expected_queue_revision.is_none());
    }
    for field in ["expected_revision", "mode", "positions"] {
        let mut value = request();
        value.as_object_mut().unwrap().remove(field);
        assert!(serde_json::from_value::<SonosPlaylistTracksRequest>(value).is_err());
    }
    for field in ["expected_revision", "expected_queue_revision"] {
        for invalid in ["".to_string(), " \n".into(), "x".repeat(4097)] {
            let mut value = request();
            value[field] = json!(invalid);
            let asked: SonosPlaylistTracksRequest = serde_json::from_value(value).unwrap();
            assert!(asked.validate().is_err(), "{field}");
        }
    }
}

#[test]
fn empty_oversize_missing_and_non_native_selections_are_rejected() {
    for positions in [vec![], vec![1; 11], vec![0], vec![1, u32::MAX]] {
        let mut value = request();
        value["positions"] = json!(positions);
        let asked: SonosPlaylistTracksRequest = serde_json::from_value(value).unwrap();
        assert!(asked.validate().is_err());
    }
    for playlist in ["1", "Morning music", "FV:2", "SQ:", "SQ:7/1"] {
        let mut value = request();
        value["playlist"] = json!(playlist);
        let asked: SonosPlaylistTracksRequest = serde_json::from_value(value).unwrap();
        assert!(asked.validate().is_err());
    }
}
