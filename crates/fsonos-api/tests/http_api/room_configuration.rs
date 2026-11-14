//! Persistent room configuration uses the selected daemon, even with no LAN.

use super::*;
use fsonos_core::store::MemStore;
use std::sync::atomic::{AtomicUsize, Ordering};

fn configured_api(client: Client) -> (tempfile::TempDir, Api, Arc<AtomicUsize>) {
    let dir = tempfile::tempdir().unwrap();
    let sent = Arc::new(Mutex::new(Vec::new()));
    let surveys = Arc::new(AtomicUsize::new(0));
    let observed = surveys.clone();
    let surface = Surface::new(
        Box::new(Canned {
            out_args: "",
            sent: sent.clone(),
        }),
        Box::new(move |_| {
            observed.fetch_add(1, Ordering::SeqCst);
            Ok(house())
        }),
        Policy::default(),
        Box::new(SystemClock),
    )
    .with_aliases_file(dir.path().join("aliases.toml"))
    .with_action_log(Box::new(MemStore::default()), "http-room-config");
    let web = WebPolicy::for_listener("127.0.0.1:0".parse().unwrap(), &[]);
    let app = fsonos_api::app(
        &Arc::new(surface),
        &fsonos_api::Identity::fixed(client),
        &web,
    );
    (dir, Api::serve(app, sent, &web), surveys)
}

fn change(configuration: &Value, edit: &Value) -> Value {
    json!({"expected_revision":configuration["revision"],"change":edit})
}

#[test]
fn http_configuration_persists_aliases_and_defaults_without_surveying_or_playback() {
    let (dir, api, surveys) = configured_api(Client::Cli);
    let (status, initial) = api.get("/rooms/configuration");
    assert_eq!(status, 200, "{initial}");
    assert_eq!(initial["aliases"], json!([]));
    assert!(initial["default_room"].is_null());
    assert_eq!(initial["client_defaults"], json!({}));
    assert_eq!(initial["warnings"][0]["line"], 0);
    let edit = json!({"kind":"set_alias","name":"downstairs","rooms":["Living Room","Kitchen"]});
    let request = change(&initial, &edit);
    let (status, saved) = api.post("/rooms/configuration", &request);
    assert_eq!(status, 200, "{saved}");
    assert_eq!(saved["changed"], true);
    let configuration = &saved["configuration"];
    assert_eq!(
        configuration["aliases"],
        json!([{"name":"downstairs","rooms":["Living Room","Kitchen"]}])
    );
    assert_eq!(
        api.get("/rooms/configuration"),
        (200, configuration.clone())
    );
    let (status, repeated) = api.post("/rooms/configuration", &change(configuration, &edit));
    assert_eq!(status, 200, "{repeated}");
    assert_eq!(repeated["changed"], false);
    assert_eq!(
        repeated["configuration"]["revision"],
        configuration["revision"]
    );
    let (status, stale) = api.post("/rooms/configuration", &request);
    assert_eq!(status, 409, "{stale}");
    assert_eq!(stale["code"], "CONFIG_CHANGED");
    let (status, personal) = api.post(
        "/rooms/configuration",
        &change(
            configuration,
            &json!({"kind":"set_default","client":"cli","room":"Kitchen"}),
        ),
    );
    assert_eq!(status, 200, "{personal}");
    assert_eq!(
        personal["configuration"]["client_defaults"]["cli"],
        "Kitchen"
    );
    assert_eq!(
        surveys.load(Ordering::SeqCst),
        0,
        "configuration does not discover speakers"
    );
    assert!(api.actions().is_empty());
    assert!(
        std::fs::read_to_string(dir.path().join("aliases.toml"))
            .unwrap()
            .contains("downstairs")
    );

    let (status, rooms) = api.get("/rooms/resolve?target=downstairs");
    assert_eq!(status, 200, "{rooms}");
    assert_eq!(rooms["rooms"].as_array().unwrap().len(), 2);
    let (status, here) = api.get("/rooms/resolve?target=here");
    assert_eq!(status, 200, "{here}");
    assert_eq!(here["rooms"][0]["room"], "Kitchen");
    assert!(
        api.actions().is_empty(),
        "resolution and edits never command speakers"
    );
}

#[test]
fn http_configuration_refuses_missing_revisions_unknown_fields_and_non_administrators() {
    let (_dir, api, surveys) = configured_api(Client::Cli);
    let (_, initial) = api.get("/rooms/configuration");
    for request in [
        json!({"change":{"kind":"clear_default"}}),
        json!({"expected_revision":initial["revision"],"change":{"kind":"clear_default","room":"Kitchen"}}),
        json!({"expected_revision":initial["revision"],"change":{"kind":"set_alias","name":"all","rooms":["Kitchen"]}}),
        json!({"expected_revision":initial["revision"],"change":{"kind":"clear_default"},"force":true}),
    ] {
        let (status, failure) = api.post("/rooms/configuration", &request);
        assert_eq!(status, 422, "{failure}");
        assert_eq!(failure["code"], "INVALID_ARGUMENT");
    }
    assert_eq!(api.get("/rooms/configuration"), (200, initial));
    assert_eq!(surveys.load(Ordering::SeqCst), 0);
    for client in [Client::Unknown, Client::LoopbackHttp] {
        let (_dir, denied, surveys) = configured_api(client);
        let (_, current) = denied.get("/rooms/configuration");
        let (status, failure) = denied.post(
            "/rooms/configuration",
            &change(
                &current,
                &json!({"kind":"set_alias","name":"downstairs","rooms":["Kitchen"]}),
            ),
        );
        assert_eq!(status, 403, "{failure}");
        assert_eq!(failure["code"], "POLICY_DENIED");
        assert_eq!(surveys.load(Ordering::SeqCst), 0);
        assert!(denied.actions().is_empty());
    }
}

#[test]
fn http_schema_exposes_the_exact_tagged_room_change_forms() {
    let (_dir, api, surveys) = configured_api(Client::Cli);
    let (status, doc) = api.get("/openapi.json");
    assert_eq!(status, 200);
    let route = &doc["paths"]["/rooms/configuration"];
    assert_eq!(route["get"]["operationId"], "room_configuration");
    assert_eq!(route["post"]["operationId"], "configure_rooms");
    let body = &route["post"]["requestBody"]["content"]["application/json"]["schema"];
    let body = body["$ref"]
        .as_str()
        .and_then(|reference| reference.rsplit('/').next())
        .map_or(body, |name| &doc["components"]["schemas"][name]);
    assert_eq!(body["required"], json!(["expected_revision", "change"]));
    let change = &body["properties"]["change"];
    let change = change["$ref"]
        .as_str()
        .and_then(|reference| reference.rsplit('/').next())
        .map_or(change, |name| &doc["components"]["schemas"][name]);
    let branches = change["oneOf"].as_array().unwrap();
    assert_eq!(branches.len(), 4);
    assert!(
        branches
            .iter()
            .all(|branch| branch["properties"]["kind"]["enum"].is_array()
                && branch["properties"].get("type").is_none())
    );
    assert_eq!(surveys.load(Ordering::SeqCst), 0);
}
