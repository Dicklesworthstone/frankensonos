//! An explicit MCP daemon selection cannot turn into local house operations.
//! The local simulator remains reachable, so fallback would be observable.

mod e2e;

use e2e::Scenario;
use fsonos_sim::SimHousehold;
use serde_json::json;

fn selected_failure(s: &mut Scenario, label: &str, args: &[&str], detail: &str) {
    let before = s.sim_handle().unwrap().soap_log().len();
    let mut session = s.mcp_with(args);
    let initialized = session.initialize();
    assert_eq!(
        initialized["result"]["protocolVersion"],
        "2024-11-05",
        "{initialized}"
    );
    for uri in ["sonos://zones", "sonos://zones/Kitchen%40S1", "sonos://dj"] {
        let answer = session.request("resources/read", &json!({ "uri": uri }));
        s.check(
            &format!("{label}-{uri}"),
            "mcp",
            "the resource preserves the selected daemon failure",
            answer["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains(detail)),
            answer.to_string(),
        );
    }
    for (name, arguments) in [
        ("set_volume", json!({ "zone": "Kitchen@S1", "volume": 29 })),
        ("events", json!({ "wait_secs": 0 })),
    ] {
        let answer = session.request(
            "tools/call",
            &json!({ "name": name, "arguments": arguments }),
        );
        s.check(
            &format!("{label}-{name}"),
            "mcp",
            "the tool preserves the selected daemon failure",
            answer["result"]["isError"] == true
                && answer["result"]["content"][0]["text"]
                    .as_str()
                    .is_some_and(|message| message.contains(detail)),
            answer.to_string(),
        );
    }
    drop(session);
    let after = s.sim_handle().unwrap().soap_log().len();
    s.check(
        &format!("{label}-no-local-operations"),
        "isolation",
        "failed daemon selection performs no local speaker reads or writes",
        after == before,
        format!("{before} SOAP calls before, {after} after"),
    );
}

#[test]
fn invalid_urls_and_failed_health_checks_never_select_the_local_house() {
    let mut s = Scenario::start("mcp-daemon-selection-errors");
    s.sim(SimHousehold::standard());
    selected_failure(
        &mut s,
        "invalid-url",
        &["--daemon-url", "not-an-http-url"],
        "--daemon-url wants an http:// or https:// URL",
    );

    // This real localhost endpoint answers HTTP but is not a daemon. Its
    // health failure must remain the cause, with the local house untouched.
    let addr = s.sim_handle().unwrap().players()[0].addr.to_string();
    let (status, _, _) = e2e::http(&addr, "GET", "/not-a-daemon/health", &[], "").unwrap();
    assert_eq!(status, 404);
    let url = format!("http://{addr}/not-a-daemon");
    selected_failure(
        &mut s,
        "failed-health",
        &["--daemon-url", &url],
        "GET /health answered 404",
    );
    s.finish();
}

#[test]
fn requiring_a_daemon_does_not_enable_standalone_when_none_is_known() {
    let mut s = Scenario::start("mcp-daemon-required");
    s.sim(SimHousehold::standard());
    selected_failure(&mut s, "required", &["--daemon"], "no daemon is known");
    s.finish();
}

#[test]
fn no_selected_daemon_still_allows_standalone_reads_and_control() {
    let mut s = Scenario::start("mcp-daemon-unselected");
    s.sim(SimHousehold::standard());
    let mut session = s.mcp();
    session.initialize();
    let zones = session.request("resources/read", &json!({ "uri": "sonos://zones" }));
    s.check(
        "standalone-zones",
        "mcp",
        "a resource can still read the local house when no daemon is selected",
        zones["error"].is_null()
            && zones["result"]["contents"][0]["text"]
                .as_str()
                .is_some_and(|text| text.contains("Kitchen")),
        zones.to_string(),
    );
    let changed = session.request(
        "tools/call",
        &json!({
            "name": "set_volume",
            "arguments": { "zone": "Kitchen@S1", "volume": 29 }
        }),
    );
    s.check(
        "standalone-volume",
        "mcp",
        "an unselected daemon still permits the local control path",
        changed["error"].is_null()
            && changed["result"]["isError"] != true
            && s.sim_handle()
                .unwrap()
                .soap_log()
                .iter()
                .any(|call| call.action == "SetVolume"),
        changed.to_string(),
    );
    drop(session);
    s.finish();
}
