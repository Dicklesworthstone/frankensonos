use super::*;
use fsonos_core::doctor::{CheckId, CheckResult, Entry, Report as CoreReport};
use serde_json::json;

fn sample() -> CoreReport {
    let entry = |id: &'static str, title: &str, result: CheckResult, duration_ms| Entry {
        id: CheckId(id),
        title: title.to_owned(),
        result: CheckResult {
            duration_ms,
            ..result
        },
    };
    CoreReport {
        entries: vec![
            entry(
                "lan.players",
                "Players",
                CheckResult::pass("3 players answered")
                    .with_evidence(json!({ "players": 3, "households": ["s1", "s2"] })),
                12,
            ),
            entry(
                "daemon.health",
                "Daemon",
                CheckResult::warn("a listener is unavailable", "Check the daemon service.")
                    .with_detail("One address did not answer.\nThe others are healthy."),
                8,
            ),
            entry(
                "spotify.account",
                "Spotify account",
                CheckResult::fail("not linked", "Link Spotify in the Sonos app."),
                5,
            ),
            entry(
                "spotify.render",
                "Spotify render parameters",
                CheckResult::skip("the account check failed"),
                0,
            ),
        ],
    }
}

fn read(value: Value, prefix: Option<&str>) -> Result<Report, Failure> {
    let report: Report = serde_json::from_value(value).unwrap();
    report.checked(prefix)
}

#[test]
fn remote_json_text_and_exit_code_match_the_core_report() {
    let core = sample();
    let remote = read(core.to_json(), None).unwrap();
    assert_eq!(remote.render_table(), core.render_table());
    assert_eq!(remote.exit_code(), core.exit_code());
    assert_eq!(serde_json::to_value(remote).unwrap(), core.to_json());
}

#[test]
fn only_recalculates_totals_and_exit_status_like_local_diagnosis() {
    for (prefix, code) in [
        (None, EXIT_FAIL),
        (Some("lan."), EXIT_OK),
        (Some("daemon."), EXIT_WARN),
        (Some("spotify."), EXIT_FAIL),
        (Some("spotify.render"), EXIT_OK),
        (Some("not-present"), EXIT_OK),
        (Some(""), EXIT_FAIL),
    ] {
        let remote = read(sample().to_json(), prefix).unwrap();
        let local = crate::doctor::only(sample(), prefix);
        assert_eq!(remote.exit_code(), code, "{prefix:?}");
        assert_eq!(remote.render_table(), local.render_table(), "{prefix:?}");
        assert_eq!(
            serde_json::to_value(remote).unwrap(),
            local.to_json(),
            "{prefix:?}"
        );
    }
}

#[test]
fn an_incompatible_report_schema_is_not_a_successful_diagnosis() {
    let mut report = sample().to_json();
    report["schema"] = json!(JSON_SCHEMA + 1);
    let error = read(report, Some("lan.")).unwrap_err();
    assert_eq!(error.code, ErrorCode::Internal);
    assert!(error.detail.contains("schema"));
}

#[test]
fn inconsistent_remote_totals_and_exit_codes_are_refused_before_filtering() {
    for (field, wrong) in [
        (
            "counts",
            json!({ "pass": 4, "warn": 0, "fail": 0, "skip": 0 }),
        ),
        ("exit_code", json!(EXIT_OK)),
        ("exit_code", json!(2)),
    ] {
        let mut report = sample().to_json();
        report[field] = wrong;
        let error = read(report, Some("lan.")).unwrap_err();
        assert_eq!(error.code, ErrorCode::Internal);
        assert!(error.detail.contains("inconsistent"));
    }
}

#[test]
fn unknown_statuses_and_malformed_checks_cannot_turn_into_passes() {
    let mut report = sample().to_json();
    report["checks"][0]["status"] = json!("unrecognized");
    assert!(serde_json::from_value::<Report>(report).is_err());

    let mut report = sample().to_json();
    report["checks"][0]["duration_ms"] = json!(-1);
    assert!(serde_json::from_value::<Report>(report).is_err());

    let mut report = sample().to_json();
    report.as_object_mut().unwrap().remove("checks");
    assert!(serde_json::from_value::<Report>(report).is_err());
}

#[test]
fn compatible_report_and_check_fields_survive_filtering() {
    let mut report = sample().to_json();
    report["generated_at"] = json!("2026-10-01T12:00:00Z");
    report["checks"][0]["additional_evidence"] = json!({ "reachable": true });
    let remote = read(report, Some("lan.")).unwrap();
    let json = serde_json::to_value(remote).unwrap();
    assert_eq!(json["generated_at"], "2026-10-01T12:00:00Z");
    assert_eq!(json["checks"][0]["additional_evidence"]["reachable"], true);
    assert_eq!(json["checks"][0]["evidence"]["players"], 3);
    assert_eq!(json["checks"].as_array().unwrap().len(), 1);
    assert_eq!(json["exit_code"], EXIT_OK);
}

fn global() -> GlobalArgs {
    GlobalArgs {
        seeds: Some("doctor-test-unused-seeds.toml".into()),
        seed: Vec::new(),
        routes: Some("doctor-test-unused-routes.toml".into()),
        wait: 0,
        json: true,
        direct: false,
        daemon: false,
        daemon_url: None,
        data_dir: None,
    }
}

#[test]
fn ordinary_and_direct_diagnosis_stay_local_but_an_explicit_url_cannot_fall_back() {
    let mut global = global();
    assert!(fetch(&global, None).unwrap().is_none());

    global.daemon_url = Some("ftp://unused".to_owned());
    global.direct = true;
    assert!(fetch(&global, None).unwrap().is_none());

    global.direct = false;
    let error = fetch(&global, None).unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidArgument);
    assert!(error.detail.contains("--daemon-url"));
}
