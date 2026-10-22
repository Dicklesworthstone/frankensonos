use super::*;
use serde_json::json;

#[test]
fn authored_direction_normalizes_all_fields_and_round_trips_without_expiry_timestamp() {
    let source: SceneSourceDto = serde_json::from_value(json!({
        "kind": "dj", "mood": " Calm ", "direction": {
            "constraints": {"include_genres": [" Jazz "], "periods": ["Late Romantic"],
                "exclude_keywords": ["live"], "energy_bias": -1},
            "for_secs": 17
        }
    }))
    .unwrap();
    let stored = source_of(&source).unwrap();
    let SceneSource::Dj {
        mood,
        direction: Some(direction),
    } = &stored
    else {
        panic!("authored direction was lost");
    };
    assert_eq!(mood.as_deref(), Some("calm"));
    assert_eq!(direction.constraints.include_genres, ["Jazz"]);
    assert_eq!(direction.constraints.periods, ["late_romantic"]);
    assert_eq!(direction.for_secs, Some(17));
    let public = SceneSourceDto::from(&stored);
    assert_eq!(source_of(&public).unwrap(), stored);
    let wire = serde_json::to_value(&public).unwrap();
    assert_eq!(wire["direction"]["for_secs"], 17);
    assert!(wire["direction"].get("expires_at").is_none());
    assert!(wire["direction"]["constraints"].get("expires_at").is_none());
    let op = SceneOp::Dj {
        coordinator: PlayerId("RINCON_TEST".into()),
        mood: mood.clone(),
        direction: Some(direction.clone()),
    };
    let described = describe(&[], &op);
    assert!(described.contains("Jazz only"));
    assert!(described.contains("for 17 seconds on each start"));
}

#[test]
fn omitted_and_null_directions_keep_legacy_while_empty_direction_remains_explicit() {
    for wire in [
        json!({"kind": "dj"}),
        json!({"kind": "dj", "direction": null}),
    ] {
        let source: SceneSourceDto = serde_json::from_value(wire).unwrap();
        assert_eq!(
            source_of(&source).unwrap(),
            SceneSource::Dj {
                mood: None,
                direction: None
            }
        );
        assert!(
            serde_json::to_value(&source)
                .unwrap()
                .get("direction")
                .is_none()
        );
    }
    let source: SceneSourceDto =
        serde_json::from_value(json!({"kind": "dj", "direction": {}})).unwrap();
    assert_eq!(
        source_of(&source).unwrap(),
        SceneSource::Dj {
            mood: None,
            direction: Some(DjDirection::default()),
        }
    );
    assert_eq!(
        serde_json::to_value(&source).unwrap(),
        json!({"kind": "dj", "direction": {}})
    );
}

#[test]
fn invalid_or_misplaced_direction_fails_pure_source_validation() {
    for source in [
        json!({"kind": "keep", "direction": {}}),
        json!({"kind": "favorite", "name": "Radio", "direction": {}}),
        json!({"kind": "uri", "uri": "https://example.test/radio", "direction": {}}),
        json!({"kind": "dj", "direction": {"for_secs": 60}}),
        json!({"kind": "dj", "direction": {"constraints": {"energy_bias": 3}}}),
        json!({"kind": "dj", "mood": " ", "direction": {}}),
        json!({"kind": "dj", "mood": "calm", "direction": {"for_secs": 0}}),
    ] {
        let definition: SceneDefinition = serde_json::from_value(json!({"groups": [{
            "coordinator": "Kitchen", "source": source,
        }]}))
        .unwrap();
        assert_eq!(
            definition.validate_sources().unwrap_err().code,
            ErrorCode::InvalidArgument
        );
    }
}
