//! Scene reviews stay bound through the real PUT/DELETE HTTP boundary.

use super::*;
use fsonos_core::clock::FakeClock;
use fsonos_core::store::MemStore;

fn scene_api(client: Client) -> Api {
    let sent = Arc::new(Mutex::new(Vec::new()));
    let surface = Surface::new(
        Box::new(Canned {
            out_args: "",
            sent: sent.clone(),
        }),
        Box::new(|_| Ok(house())),
        Policy::default(),
        Box::new(FakeClock::new(
            chrono::DateTime::parse_from_rfc3339("2026-10-08T12:00:00Z").unwrap(),
        )),
    )
    .with_action_log(Box::new(MemStore::default()), "http-scene-revisions");
    let web = WebPolicy::for_listener("127.0.0.1:0".parse().unwrap(), &[]);
    let app = fsonos_api::app(
        &Arc::new(surface),
        &fsonos_api::Identity::fixed(client),
        &web,
    );
    Api::serve(app, sent, &web)
}

fn write(api: &Api, method: &str, name: &str, body: &Value) -> (u16, Value) {
    let host = api.host();
    let (status, raw) = api.raw(
        method,
        &format!("/scenes/{name}"),
        &[("Host", &host), ("Content-Type", "application/json")],
        &body.to_string(),
    );
    (status, serde_json::from_str(&raw).unwrap())
}

#[test]
fn reviewed_scene_writes_and_deletes_reject_stale_same_second_revisions() {
    let api = scene_api(Client::LoopbackHttp);
    let (status, saved) = write(
        &api,
        "PUT",
        "Evening",
        &json!({"volumes":{"Kitchen":21},"expected_revision":"absent"}),
    );
    assert_eq!(status, 200, "{saved}");
    assert!(!saved["revision"].as_str().unwrap().is_empty());
    assert_eq!(api.get("/scenes/Evening"), (200, saved.clone()));
    assert_eq!(api.get("/scenes"), (200, json!([saved])));

    let (status, changed) = write(
        &api,
        "PUT",
        "EVENING",
        &json!({"volumes":{"Kitchen":32},"expected_revision":saved["revision"]}),
    );
    assert_eq!(status, 200, "{changed}");
    assert_eq!(changed["saved"], saved["saved"]);
    assert_ne!(changed["revision"], saved["revision"]);
    for (method, body) in [
        (
            "PUT",
            json!({"volumes":{"Kitchen":44},"expected_revision":saved["revision"]}),
        ),
        ("PUT", json!({"expected_revision":"absent"})),
        (
            "PUT",
            json!({"backup_queues":true,"expected_revision":"absent"}),
        ),
        ("DELETE", json!({"expected_revision":saved["revision"]})),
    ] {
        let (status, conflict) = write(&api, method, "Evening", &body);
        assert_eq!(status, 409, "{method}: {conflict}");
        assert_eq!(conflict["code"], "CONFIG_CHANGED");
        assert_eq!(conflict["retryable"], false);
    }
    assert_eq!(api.get("/scenes"), (200, json!([changed])));
    let (status, deleted) = write(
        &api,
        "DELETE",
        "evening",
        &json!({"expected_revision":changed["revision"]}),
    );
    assert_eq!(status, 200, "{deleted}");
    assert_eq!(deleted["deleted"], "EVENING");
    assert_eq!(api.get("/scenes"), (200, json!([])));
    assert!(
        api.actions().is_empty(),
        "storage changes and stale capture reviews never read speakers"
    );
}

#[test]
fn legacy_scene_requests_remain_accepted_and_invalid_proofs_are_rejected() {
    let api = scene_api(Client::LoopbackHttp);
    let (status, saved) = write(&api, "PUT", "Evening", &json!({"volumes":{"Kitchen":21}}));
    assert_eq!(status, 200, "{saved}");
    for body in [
        json!({"expected_revision":""}),
        json!({"expected_revision":"bad\nrevision"}),
        json!({"expected_revision":42}),
        json!({"expected_revision":"known","writer":"cli"}),
    ] {
        for method in ["PUT", "DELETE"] {
            let (status, error) = write(&api, method, "Evening", &body);
            assert_eq!(status, 422, "{method}: {error}");
            assert_eq!(error["code"], "INVALID_ARGUMENT");
        }
    }
    let (status, changed) = write(&api, "PUT", "Evening", &json!({"volumes":{"Kitchen":22}}));
    assert_eq!(status, 200, "{changed}");
    let host = api.host();
    let (status, deleted) = api.raw("DELETE", "/scenes/Evening", &[("Host", &host)], "");
    assert_eq!(status, 200, "{deleted}");
    assert!(api.actions().is_empty());

    let denied = scene_api(Client::Unknown);
    for (method, body) in [
        (
            "PUT",
            json!({"volumes":{"Kitchen":21},"expected_revision":"absent"}),
        ),
        ("DELETE", json!({"expected_revision":"observed"})),
    ] {
        let (status, failure) = write(&denied, method, "Evening", &body);
        assert_eq!(status, 403, "{failure}");
        assert_eq!(failure["code"], "POLICY_DENIED");
    }
    assert!(denied.actions().is_empty());
}

fn resolve<'a>(doc: &'a Value, schema: &'a Value) -> &'a Value {
    match schema["$ref"].as_str() {
        Some(reference) => resolve(
            doc,
            doc.pointer(reference.strip_prefix('#').unwrap()).unwrap(),
        ),
        None => schema,
    }
}

#[test]
fn scene_route_schemas_advertise_optional_revision_guards_and_returned_proofs() {
    let api = scene_api(Client::LoopbackHttp);
    let (status, doc) = api.get("/openapi.json");
    assert_eq!(status, 200);
    for method in ["put", "delete"] {
        let body = &doc["paths"]["/scenes/{name}"][method]["requestBody"];
        assert_ne!(body["required"], true);
        let schema = resolve(&doc, &body["content"]["application/json"]["schema"]);
        assert!(
            schema["properties"]["expected_revision"].is_object(),
            "{method}: {schema}"
        );
        assert!(
            !schema["required"]
                .as_array()
                .is_some_and(|fields| fields.contains(&json!("expected_revision")))
        );
    }
    for method in ["get", "put"] {
        let response = &doc["paths"]["/scenes/{name}"][method]["responses"]["200"]["content"]["application/json"]
            ["schema"];
        let schema = resolve(&doc, response);
        assert_eq!(
            schema["properties"]["revision"]["type"], "string",
            "{schema}"
        );
    }
}
