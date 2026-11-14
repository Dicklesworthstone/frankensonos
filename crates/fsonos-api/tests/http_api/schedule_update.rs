//! Revision-bound schedule editing over the real HTTP server.

use super::*;
use fsonos_core::clock::FakeClock;
use fsonos_core::store::MemStore;

fn schedule_api(client: Client) -> Api {
    let sent = Arc::new(Mutex::new(Vec::new()));
    let surface = Surface::new(
        Box::new(Canned {
            out_args: "",
            sent: Arc::clone(&sent),
        }),
        Box::new(|_| Ok(house())),
        Policy::default(),
        Box::new(FakeClock::new(
            chrono::DateTime::parse_from_rfc3339("2026-10-08T12:00:00Z").unwrap(),
        )),
    )
    .with_action_log(Box::new(MemStore::default()), "http-schedule-edit");
    let web = WebPolicy::for_listener("127.0.0.1:0".parse().unwrap(), &[]);
    let app = fsonos_api::app(
        &Arc::new(surface),
        &fsonos_api::Identity::fixed(client),
        &web,
    );
    Api::serve(app, sent, &web)
}

fn pause(when: &str) -> Value {
    json!({"when":when,"action":"pause","zone":"Living Room"})
}

fn update(observed: &Value, schedule: &Value) -> Value {
    json!({
        "id":observed["id"],
        "expected_revision":observed["revision"],
        "schedule":schedule
    })
}

#[test]
fn editing_over_http_preserves_a_paused_routine_and_rejects_its_old_revision() {
    let api = schedule_api(Client::LoopbackHttp);
    let (status, added) = api.post("/schedules", &pause("daily 09:30"));
    assert_eq!(status, 200, "{added}");
    assert!(!added["revision"].as_str().unwrap().is_empty());
    let selected = json!({"id":added["id"]});
    let (status, paused) = api.post("/schedules/pause", &selected);
    assert_eq!(status, 200, "{paused}");
    assert_ne!(paused["revision"], added["revision"]);
    let request = update(
        &paused,
        &json!({"when":"in 45m","action":"volume","zone":"Kitchen","volume":23}),
    );
    let (status, edited) = api.post("/schedules/update", &request);
    assert_eq!(status, 200, "{edited}");
    assert_eq!(edited["id"], added["id"]);
    assert_eq!(edited["creator"], added["creator"]);
    assert_eq!(edited["enabled"], false);
    assert!(edited.get("next").is_none());
    assert_eq!(edited["zone"], "Kitchen");
    assert_eq!(edited["volume"], 23);
    assert_ne!(edited["revision"], paused["revision"]);
    assert_eq!(api.get("/schedules"), (200, json!([edited])));

    let (status, conflict) = api.post("/schedules/update", &request);
    assert_eq!(status, 409, "{conflict}");
    assert_eq!(conflict["code"], "CONFIG_CHANGED");
    let (status, resumed) = api.post("/schedules/resume", &selected);
    assert_eq!(status, 200, "{resumed}");
    assert_eq!(resumed["enabled"], true);
    let next = chrono::DateTime::parse_from_rfc3339(resumed["next"].as_str().unwrap()).unwrap();
    let expected = chrono::DateTime::parse_from_rfc3339("2026-10-08T12:45:00Z").unwrap();
    assert_eq!(next, expected, "relative edits use the daemon clock");
    let same = update(
        &resumed,
        &json!({"when":resumed["when"],"action":"volume","zone":"Kitchen","volume":23}),
    );
    assert_eq!(api.post("/schedules/update", &same), (200, resumed));
    assert_eq!(api.post("/schedules/remove", &selected).0, 200);
    let (status, missing) = api.post("/schedules/update", &same);
    assert_eq!(status, 404, "{missing}");
    assert_eq!(missing["code"], "UNKNOWN_SCHEDULE");
    assert!(api.actions().is_empty(), "editing never commands speakers");
}

#[test]
fn update_rejects_unreviewed_or_injected_definitions_and_denied_callers() {
    let api = schedule_api(Client::LoopbackHttp);
    let (_, added) = api.post("/schedules", &pause("daily 09:30"));
    let valid = update(&added, &pause("daily 10:00"));
    let mut missing_revision = valid.clone();
    missing_revision
        .as_object_mut()
        .unwrap()
        .remove("expected_revision");
    let mut injected_owner = valid.clone();
    injected_owner["schedule"]["creator"] = json!("cli");
    let mut force = valid.clone();
    force["force"] = json!(true);
    let mut malformed = valid.clone();
    malformed["schedule"]["action"] = json!("volume");
    for request in [missing_revision, injected_owner, force, malformed] {
        let (status, failure) = api.post("/schedules/update", &request);
        assert_eq!(status, 422, "{failure}");
        assert_eq!(failure["code"], "INVALID_ARGUMENT");
    }
    assert_eq!(api.get("/schedules"), (200, json!([added])));
    assert!(api.actions().is_empty());

    let denied = schedule_api(Client::Unknown);
    let (status, failure) = denied.post("/schedules/update", &valid);
    assert_eq!(status, 403, "{failure}");
    assert_eq!(failure["code"], "POLICY_DENIED");
    assert!(denied.actions().is_empty());
}

#[test]
fn update_publishes_a_typed_revision_bound_http_contract() {
    let api = schedule_api(Client::LoopbackHttp);
    let (status, doc) = api.get("/openapi.json");
    assert_eq!(status, 200);
    let route = &doc["paths"]["/schedules/update"]["post"];
    assert_eq!(route["operationId"], "update_schedule");
    let body = &route["requestBody"]["content"]["application/json"]["schema"];
    let body = body["$ref"]
        .as_str()
        .and_then(|reference| reference.rsplit('/').next())
        .map_or(body, |name| &doc["components"]["schemas"][name]);
    assert_eq!(
        body["required"],
        json!(["id", "expected_revision", "schedule"])
    );
    assert!(body["properties"]["schedule"].is_object());
    let response = &route["responses"]["200"]["content"]["application/json"]["schema"];
    let response = response["$ref"]
        .as_str()
        .and_then(|reference| reference.rsplit('/').next())
        .map_or(response, |name| &doc["components"]["schemas"][name]);
    assert_eq!(response["properties"]["revision"]["type"], "string");
    assert!(route["responses"]["409"].is_object());
}
