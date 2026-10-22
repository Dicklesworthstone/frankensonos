use super::*;
use crate::failure::ErrorCode;
use serde_json::json;

#[test]
fn typed_conversion_preserves_every_filter_and_relative_lifetime() {
    let wire = json!({"constraints": {
        "include_composers": ["Bach"], "exclude_composers": ["Wagner"],
        "include_artists": ["Miles Davis"], "exclude_artists": ["Other artist"],
        "include_genres": ["jazz"], "exclude_genres": ["metal"],
        "periods": ["baroque"], "exclude_periods": ["late_romantic"],
        "decades": [1960], "exclude_decades": [1980],
        "include_keywords": ["piano"], "exclude_keywords": ["opera"],
        "min_work_minutes": 2, "max_work_minutes": 45,
        "energy_bias": -1, "allow_long_works": true
    }, "for_secs": 7200});
    let direction: DjDirectionDto = serde_json::from_value(wire.clone()).unwrap();
    direction.validate(Some("calm")).unwrap();
    let core = DjDirection::from(&direction);
    assert_eq!(serde_json::to_value(&core).unwrap(), wire);
    assert_eq!(DjDirectionDto::from(&core), direction);
    assert_eq!(serde_json::to_value(&direction).unwrap(), wire);
}

#[test]
fn empty_direction_is_an_explicit_clear_and_unknown_fields_are_rejected() {
    let empty: DjDirectionDto = serde_json::from_value(json!({})).unwrap();
    assert_eq!(empty.steer(None).unwrap(), DjSteer::Clear);
    assert_eq!(empty.summary(None), "automatic steering");
    assert_eq!(serde_json::to_value(&empty).unwrap(), json!({}));
    assert!(matches!(
        empty.steer(Some("Calm")).unwrap(),
        DjSteer::Set { mood: Some(mood), for_secs: None, .. } if mood == "calm"
    ));
    for wire in [
        json!({"expires_at": 123}),
        json!({"constraints": {"expires_at": 123}}),
        json!({"constraints": {"include_genre": ["jazz"]}}),
        json!({"for_secs": -1}),
    ] {
        assert!(serde_json::from_value::<DjDirectionDto>(wire).is_err());
    }
}

#[test]
fn remaining_seconds_are_exact_and_reuse_start_constraint_validation() {
    let direction = DjDirectionDto {
        constraints: SteerConstraints {
            include_artists: vec![" Miles Davis ".into()],
            periods: vec![" Late-Romantic ".into()],
            ..SteerConstraints::default()
        },
        for_secs: Some(17),
    };
    let (mood, normalized) = direction.normalized(Some(" Calm ")).unwrap();
    assert_eq!(mood.as_deref(), Some("calm"));
    assert_eq!(normalized.constraints.include_artists, ["Miles Davis"]);
    assert_eq!(normalized.constraints.periods, ["late_romantic"]);
    assert_eq!(normalized.for_secs, Some(17));
    assert!(
        normalized
            .summary(mood.as_deref())
            .contains("for 17 seconds on each start")
    );
    for secs in [1, 59, 60, MAX_STEER_SECS] {
        DjDirectionDto {
            for_secs: Some(secs),
            ..direction.clone()
        }
        .validate(None)
        .unwrap();
    }
    for secs in [0, MAX_STEER_SECS + 1, u64::MAX] {
        let error = DjDirectionDto {
            for_secs: Some(secs),
            ..direction.clone()
        }
        .validate(None)
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidArgument);
    }
    assert!(
        DjDirectionDto {
            for_secs: Some(60),
            ..DjDirectionDto::default()
        }
        .validate(None)
        .is_err()
    );
    let invalid = DjDirectionDto {
        constraints: SteerConstraints {
            energy_bias: 3,
            ..SteerConstraints::default()
        },
        for_secs: None,
    };
    assert!(invalid.validate(None).is_err());
    assert!(direction.validate(Some(" ")).is_err());
}
