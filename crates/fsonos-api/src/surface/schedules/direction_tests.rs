use super::*;
use serde_json::json;

fn request(direction: serde_json::Value) -> ScheduleRequest {
    serde_json::from_value(json!({
        "when": "daily 07:30", "action": "dj_start", "zone": " Kitchen ",
        "direction": direction,
    }))
    .unwrap()
}

#[test]
fn scheduled_direction_is_normalized_and_reported_as_a_relative_lifetime() {
    let request = request(json!({"constraints": {
        "include_artists": [" Miles Davis "], "exclude_genres": ["metal"],
        "decades": [1960], "max_work_minutes": 20, "energy_bias": -1,
    }, "for_secs": 900}));
    request.validate().unwrap();
    let action = request.action().unwrap();
    let ScheduleAction::DjStart {
        room,
        mood,
        direction: Some(direction),
    } = &action
    else {
        panic!("scheduled direction was lost");
    };
    assert_eq!(room, "Kitchen");
    assert_eq!(mood, &None);
    assert_eq!(direction.constraints.include_artists, ["Miles Davis"]);
    assert_eq!(direction.for_secs, Some(900));
    let now = DateTime::parse_from_rfc3339("2026-10-08T07:00:00+00:00").unwrap();
    let schedule = Schedule {
        id: 1,
        spec: ScheduleSpec::parse(&request.when, now).unwrap(),
        action,
        creator: "cli".into(),
        enabled: true,
        created: now.to_utc(),
        last_fired: None,
    };
    let dto = ScheduleDto::new(&schedule, now.to_utc());
    assert_eq!(dto.direction.as_ref().unwrap().for_secs, Some(900));
    assert!(dto.summary.contains("Miles Davis"));
    assert!(dto.summary.contains("for 15 minutes on each start"));
    let wire = serde_json::to_value(&dto).unwrap();
    assert!(wire["direction"].get("expires_at").is_none());
    assert_eq!(serde_json::from_value::<ScheduleDto>(wire).unwrap(), dto);
}

#[test]
fn automatic_is_distinct_from_legacy_start_and_cannot_have_a_lifetime() {
    let legacy = request(serde_json::Value::Null).action().unwrap();
    assert_eq!(
        legacy,
        ScheduleAction::DjStart {
            room: "Kitchen".into(),
            mood: None,
            direction: None,
        }
    );
    assert_eq!(summary(&legacy), "start the DJ in Kitchen");
    let automatic = request(json!({})).action().unwrap();
    assert_eq!(
        automatic,
        ScheduleAction::DjStart {
            room: "Kitchen".into(),
            mood: None,
            direction: Some(DjDirection::default()),
        }
    );
    assert!(summary(&automatic).contains("automatic steering"));
    assert_eq!(
        request(json!({"for_secs": 900}))
            .validate()
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
}

#[test]
fn non_dj_actions_refuse_direction_before_any_io() {
    for action in ["pause", "volume", "apply_scene"] {
        let mut request = request(json!({}));
        request.action = action.into();
        match action {
            "volume" => request.volume = Some(20),
            "apply_scene" => {
                request.zone = None;
                request.scene = Some("dinner".into());
            }
            _ => {}
        }
        let failure = request.validate().unwrap_err();
        assert_eq!(failure.code, ErrorCode::InvalidArgument);
        assert!(failure.detail.contains("direction"));
    }
    for direction in [
        json!({"expires_at": 123}),
        json!({"constraints": {"song_keywords": ["typo"]}}),
    ] {
        assert!(
            serde_json::from_value::<ScheduleRequest>(json!({
                "when": "daily 07:30", "action": "dj_start", "zone": "Kitchen",
                "direction": direction,
            }))
            .is_err()
        );
    }
}
