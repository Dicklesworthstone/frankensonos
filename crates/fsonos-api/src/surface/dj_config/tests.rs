use super::*;
use crate::ErrorCode;
use serde_json::{Value, json};

fn request(config: Value) -> DjConfigureRequest {
    serde_json::from_value(json!({"expected_revision": "a".repeat(64), "config": config})).unwrap()
}

#[test]
fn default_program_and_explicitly_disabled_program_remain_distinct() {
    let defaults = request(json!({})).checked().unwrap();
    assert_eq!(defaults.config.programs, None);
    let off = request(json!({"programs": []})).checked().unwrap();
    assert_eq!(off.config.programs, Some(vec![]));
    assert_ne!(defaults, off);
}

#[test]
fn stored_moods_roundtrip_song_alternatives_expiry_and_strict_filters() {
    let original = request(json!({"moods": {"Evening": {
        "periods": ["Late Romantic"], "song_keywords": [" soul ", "gospel"],
        "include_artists": [" Alice "], "energy_bias": -2,
        "strict": true, "allow_long_works": true, "expires_at": 1900000000,
        "decades": [1970], "max_work_minutes": 24
    }}}));
    let checked = original.checked().unwrap();
    let mood = &checked.config.moods["evening"];
    assert_eq!(mood.periods, ["late_romantic"]);
    assert_eq!(mood.song_keywords, [" soul ", "gospel"]);
    assert_eq!(mood.include_artists, [" Alice "]);
    assert_eq!(mood.expires_at, Some(1900000000));
    assert!(mood.strict && mood.allow_long_works);
    let copy: DjConfigureRequest =
        serde_json::from_slice(&serde_json::to_vec(&checked).unwrap()).unwrap();
    assert_eq!(copy, checked);
}

#[test]
fn program_precedence_overnight_and_full_day_are_preserved() {
    let checked = request(json!({"programs": [
        {"days":["FRI", "mon"],"from":"21:00","to":"06:00","mood":"Calm"},
        {"days":["mon"],"from":"00:00","to":"24:00","mood":"focus"},
        {"days":["sun"],"from":"12:00","to":"12:00","mood":"dinner"}
    ]}))
    .checked()
    .unwrap();
    let programs = checked.config.programs.unwrap();
    assert_eq!(programs[0].days, ["mon", "fri"]);
    assert_eq!(
        (&*programs[0].from, &*programs[0].to, &*programs[0].mood),
        ("21:00", "06:00", "calm")
    );
    assert_eq!(programs[1].to, "24:00");
    assert_eq!(programs[2].from, programs[2].to);
}

#[test]
fn rejects_stale_token_shapes_ambiguous_names_and_unbounded_or_invalid_rules() {
    let mut bad = request(json!({}));
    for revision in ["", "missing", &"A".repeat(64), &"a".repeat(65)] {
        bad.expected_revision = revision.into();
        assert_eq!(bad.checked().unwrap_err().code, ErrorCode::InvalidArgument);
    }
    for config in [
        json!({"moods": {"Dinner": {}, "dinner": {}}}),
        json!({"moods": {"\n": {}}}),
        json!({"moods": {"custom": {"song_keywords": vec!["word"; 33]}}}),
        json!({"moods": {"custom": {"energy_bias": 3}}}),
        json!({"moods": {"custom": {"min_work_minutes": 30, "max_work_minutes": 10}}}),
        json!({"programs": [{"days":[],"from":"00:00","to":"24:00","mood":"focus"}]}),
        json!({"programs": [{"days":["mon", "MON"],"from":"00:00","to":"24:00","mood":"focus"}]}),
        json!({"programs": [{"days":["wed"],"from":"24:01","to":"00:00","mood":"focus"}]}),
        json!({"programs": [{"days":["daily"],"from":"00:00","to":"24:00","mood":"focus"}]}),
    ] {
        assert_eq!(
            request(config.clone()).checked().unwrap_err().code,
            ErrorCode::InvalidArgument,
            "{config}"
        );
    }
}

#[test]
fn unrecognized_fields_never_disappear_during_an_edit() {
    for config in [
        json!({"moods": {"focus": {"future_rule": true}}}),
        json!({"future_configuration": {}}),
        json!({"programs": [{"days":["mon"],"from":"00:00","to":"24:00","mood":"focus","zone":"Kitchen"}]}),
    ] {
        assert!(
            serde_json::from_value::<DjConfigureRequest>(json!({
                "expected_revision": "a".repeat(64), "config": config
            }))
            .is_err()
        );
    }
}

#[test]
fn config_conflicts_have_a_distinct_non_retryable_contract() {
    assert_eq!(ErrorCode::ConfigChanged.status(), 409);
    assert!(!ErrorCode::ConfigChanged.retryable());
    assert_eq!(ErrorCode::ConfigChanged.as_str(), "CONFIG_CHANGED");
}

#[test]
fn program_only_edits_preserve_existing_authored_names_and_filters() {
    let checked = request(
        json!({"moods": {" evening ": {"include_keywords": [" piano "]}},
            "programs": [{"days":["fri"],"from":"21:00","to":"02:00","mood":" evening "}]
        }),
    )
    .checked()
    .unwrap();
    assert_eq!(
        checked.config.moods[" evening "].include_keywords,
        [" piano "]
    );
    assert_eq!(checked.config.programs.unwrap()[0].mood, " evening ");
}

#[test]
fn configuration_writes_need_the_dj_grant_before_any_engine_or_store_access() {
    use fsonos_core::clock::SystemClock;
    use fsonos_core::policy::Policy;
    use fsonos_proto::{ProtoError, Transport};
    use std::net::IpAddr;
    struct NoLan;
    impl Transport for NoLan {
        fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
            panic!("configuration must not contact speakers")
        }
    }
    for (grant, expected) in [
        ("read", ErrorCode::PolicyDenied),
        ("dj", ErrorCode::NotImplemented),
    ] {
        let policy =
            Policy::from_toml(&format!("[clients.mcp-stdio]\nallow = [\"{grant}\"]\n")).unwrap();
        let surface = Surface::new(
            Box::new(NoLan),
            Box::new(|_| panic!("configuration must not discover speakers")),
            policy,
            Box::new(SystemClock),
        );
        // No engine is installed, so an authorized call reaches the capability
        // error; a read-only call must fail at policy first.
        assert_eq!(
            surface
                .dj_configure(&Client::McpStdio, &request(json!({})))
                .unwrap_err()
                .code,
            expected
        );
        assert_eq!(
            surface.dj_configuration(&Client::Unknown).unwrap_err().code,
            ErrorCode::NotImplemented
        );
    }
}
