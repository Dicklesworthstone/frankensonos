use super::*;
use serde_json::json;

#[test]
fn single_response_stays_wire_compatible_and_batch_cannot_be_mistaken_for_it() {
    let single = json!({
        "done": "queued one track", "zone": "Room", "first": 3,
        "count": 1, "playing_queue": true
    });
    let outcome: QueueAddOutcome = serde_json::from_value(single.clone()).unwrap();
    assert!(matches!(outcome, QueueAddOutcome::Single(_)));
    assert_eq!(serde_json::to_value(&outcome).unwrap(), single);
    assert_eq!(outcome.text(), "queued one track\n");

    let batch = json!({
        "done": "queued in 0 of 2 groups; incomplete",
        "complete": false, "added": [], "failed": [],
        "logged": true, "undoable": false
    });
    let outcome: QueueAddOutcome = serde_json::from_value(batch.clone()).unwrap();
    assert!(matches!(outcome, QueueAddOutcome::Groups(_)));
    assert_eq!(serde_json::to_value(&outcome).unwrap(), batch);
    assert!(serde_json::from_value::<QueueAddedDto>(batch).is_err());
}

#[test]
fn set_text_preserves_qualified_successes_failures_and_undo_limitations() {
    let outcome = QueueAddOutcome::Groups(QueueAddedSetDto {
        done: "queued in 1 of 2 groups; incomplete".into(),
        complete: false,
        added: vec![QueueAddedTargetDto {
            household: "S1".into(),
            coordinator: "test:one".into(),
            queue: QueueAddedDto {
                done: "queued 3 tracks at position 4".into(),
                zone: "Room".into(),
                first: 4,
                count: 3,
                playing_queue: true,
            },
        }],
        failed: vec![QueueTargetFailureDto {
            household: "S2".into(),
            coordinator: "test:two".into(),
            zone: "Room".into(),
            error: Failure::new(ErrorCode::OutcomeUncertain, "the add reply was lost").body(),
        }],
        logged: true,
        undoable: false,
    });
    let text = outcome.text();
    for expected in [
        "incomplete",
        "Room@S1: queued 3 tracks at position 4",
        "Room@S2: OUTCOME_UNCERTAIN: the add reply was lost",
        "No safe batch undo",
    ] {
        assert!(text.contains(expected), "{text}");
    }
}

#[test]
fn outcome_schema_matches_untagged_wire_variants() {
    let schema =
        serde_json::to_value(<QueueAddOutcome as fastapi_openapi::JsonSchema>::schema()).unwrap();
    let variants = schema["oneOf"].as_array().unwrap();
    assert_eq!(variants.len(), 2);
    assert!(variants[0]["properties"].get("first").is_some());
    assert!(variants[1]["properties"].get("added").is_some());
    assert!(variants[1]["properties"].get("first").is_none());
    for variant in variants {
        assert!(variant["properties"].get("type").is_none());
        assert!(variant["properties"].get("data").is_none());
    }
}
