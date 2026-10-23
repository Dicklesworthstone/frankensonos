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
        json!({"kind": "queue_backup"}),
        json!({"kind": "queue", "queue": queue}),
        json!({"kind": "queue_backup", "queue": queue}),
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

#[test]
fn backup_projection_exposes_count_without_raw_track_metadata() {
    let backup = scenes::QueueBackup {
        reference: reference(),
        entries: (0..4)
            .map(|_| scenes::QueueBackupEntry {
                uri: "x-file-cifs://example.invalid/private-track.flac".into(),
                metadata: "private synthetic metadata".into(),
            })
            .collect(),
    };
    let source = SceneSourceDto::from(&SceneSource::QueueBackup { backup });
    let wire = serde_json::to_value(&source).unwrap();
    assert_eq!(wire["kind"], "queue_backup");
    assert_eq!(wire["queue"]["total"], 4);
    assert_eq!(wire["queue"]["track"], 3);
    assert!(!wire.to_string().contains("private"));
    let text = source.queue.unwrap().text();
    assert!(text.contains("4 entries"));
    assert!(text.contains("replaces the original player's current queue"));
}

#[test]
fn queue_backup_option_is_only_valid_for_capture() {
    for value in [
        json!({}),
        json!({"backup_queues": true}),
        json!({"backup_queues": false}),
    ] {
        let request: SceneSaveRequest = serde_json::from_value(value).unwrap();
        assert!(request.into_definition().unwrap().is_none());
    }
    for value in [
        json!({"backup_queues": true, "groups": [{"coordinator": "Kitchen"}]}),
        json!({"backup_queues": true, "volumes": {"Kitchen": 20}}),
        json!({"backup_queues": true, "mutes": {"Kitchen": false}}),
    ] {
        let request: SceneSaveRequest = serde_json::from_value(value).unwrap();
        assert_eq!(
            request.into_definition().unwrap_err().code,
            ErrorCode::InvalidArgument
        );
    }
    let authored: SceneSaveRequest =
        serde_json::from_value(json!({"volumes": {"Kitchen": 20}})).unwrap();
    assert_eq!(
        authored.into_definition().unwrap().unwrap().volumes["Kitchen"],
        20
    );
    assert!(serde_json::from_value::<SceneSaveRequest>(json!({"backup_queues": "yes"})).is_err());
}
