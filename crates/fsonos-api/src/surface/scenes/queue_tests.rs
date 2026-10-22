use super::*;
use fsonos_types::{HouseholdId, PlayMode};
use serde_json::json;

fn reference() -> scenes::QueueReference {
    scenes::QueueReference {
        coordinator: PlayerId("RINCON_TEST".into()),
        household: Some(HouseholdId("TEST-HOUSE".into())),
        update_id: 42,
        track: 3,
        position_secs: 83,
        play_mode: Some(PlayMode::ShuffleNoRepeat),
        boot_seq: Some(7),
    }
}

#[test]
fn captured_queue_wire_keeps_occurrence_identity_and_known_play_mode() {
    let source = SceneSourceDto::from(&SceneSource::Queue { queue: reference() });
    let wire = serde_json::to_value(&source).unwrap();
    assert_eq!(
        wire,
        json!({"kind": "queue", "queue": {
            "coordinator": "RINCON_TEST", "household": "TEST-HOUSE",
            "update_id": 42, "track": 3, "position_secs": 83,
            "play_mode": "SHUFFLE_NOREPEAT", "boot_seq": 7
        }})
    );
    assert_eq!(
        serde_json::from_value::<SceneSourceDto>(wire).unwrap(),
        source
    );
    let text = source.queue.unwrap().text();
    assert!(text.contains("track 3 at 1:23, SHUFFLE_NOREPEAT"));
    assert!(text.contains("original unchanged queue"));
    assert!(text.contains("queue contents are not saved"));
}

#[test]
fn legacy_sources_and_unknown_optional_queue_state_remain_compatible() {
    let keep: SceneSourceDto = serde_json::from_value(json!({"kind": "keep"})).unwrap();
    assert!(keep.queue.is_none());
    assert_eq!(
        serde_json::to_value(&keep).unwrap(),
        json!({"kind": "keep"})
    );
    assert_eq!(source_of(&keep).unwrap(), SceneSource::Keep);

    let queue = scenes::QueueReference {
        household: None,
        play_mode: None,
        boot_seq: None,
        ..reference()
    };
    let source = SceneSourceDto::from(&SceneSource::Queue { queue });
    let wire = serde_json::to_value(&source).unwrap();
    assert!(wire["queue"].get("household").is_none());
    assert!(wire["queue"].get("play_mode").is_none());
    assert!(wire["queue"].get("boot_seq").is_none());
    assert_eq!(
        serde_json::from_value::<SceneSourceDto>(wire).unwrap(),
        source
    );
}

#[test]
fn authored_queue_references_are_rejected_even_under_another_kind() {
    let queue = serde_json::to_value(SceneQueueDto::from(&reference())).unwrap();
    for source in [
        json!({"kind": "queue"}),
        json!({"kind": "queue", "queue": queue}),
        json!({"kind": "keep", "queue": queue}),
        json!({"kind": "uri", "uri": "https://example.test/music", "queue": queue}),
    ] {
        let definition: SceneDefinition = serde_json::from_value(json!({
            "groups": [{"coordinator": "Kitchen", "source": source}]
        }))
        .unwrap();
        let failure = definition.validate_sources().unwrap_err();
        assert_eq!(failure.code, ErrorCode::InvalidArgument);
        assert!(failure.detail.contains("only be captured"));
        assert!(failure.hint.contains("without --from"));
    }
}
