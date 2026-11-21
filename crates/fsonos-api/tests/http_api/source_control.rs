//! The browser's reviewed source contract is enforced by the actual HTTP route.

use super::*;

#[test]
fn reviewed_source_http_rejects_stale_audiences_before_speaker_io() {
    let api = Api::start("<CurrentVolume>20</CurrentVolume>", &Client::LoopbackHttp);
    let (status, resolved) = api.get("/rooms/resolve?target=Kitchen");
    assert_eq!(status, 200, "{resolved}");
    let request = json!({
        "zone": "Kitchen",
        "expected_revision": "0".repeat(64),
        "mode": "play",
        "source": {"kind": "uri", "source_uri": "https://audio.example.invalid/live"},
    });
    let (status, failure) = api.post("/sources/control", &request);
    assert_eq!(status, 409, "{failure}");
    assert_eq!(failure["code"], "NOT_COORDINATOR");
    assert!(api.actions().is_empty());

    let mut request = request;
    request["expected_revision"] = resolved["revision"].clone();
    let (status, played) = api.post("/sources/control", &request);
    assert_eq!(status, 200, "{played}");
    assert_eq!(played["changed"], true);
    assert_eq!(
        api.actions()
            .iter()
            .filter(|action| *action == "SetAVTransportURI")
            .count(),
        1
    );
    assert_eq!(
        api.actions()
            .iter()
            .filter(|action| *action == "Play")
            .count(),
        1
    );
}

#[test]
fn reviewed_source_http_requires_a_revision_and_a_typed_exclusive_source() {
    let api = Api::start("", &Client::LoopbackHttp);
    for source in [
        json!({"kind":"favorite","favorite":"Morning","source_uri":"untrusted"}),
        json!({"kind":"playlist","playlist":"Evening","metadata":"untrusted"}),
        json!({"kind":"line_in","source":"unreviewed"}),
    ] {
        let (status, failure) = api.post(
            "/sources/control",
            &json!({
                "zone":"Kitchen","expected_revision":"a".repeat(64),"mode":"play","source":source,
            }),
        );
        assert_eq!(status, 422, "{failure}");
        assert_eq!(failure["code"], "INVALID_ARGUMENT");
    }
    let (status, failure) = api.post(
        "/sources/control",
        &json!({
            "zone":"Kitchen","mode":"play","source":{"kind":"favorite","favorite":"Morning"},
        }),
    );
    assert_eq!(status, 422, "{failure}");
    assert!(api.actions().is_empty());
}

#[test]
fn reviewed_source_openapi_documents_modes_target_revision_and_both_receipts() {
    let api = Api::start("", &Client::LoopbackHttp);
    let (_, doc) = api.get("/openapi.json");
    let op = &doc["paths"]["/sources/control"]["post"];
    assert_eq!(op["operationId"], "source_control");
    let schema = &op["requestBody"]["content"]["application/json"]["schema"];
    let schema = schema["$ref"]
        .as_str()
        .and_then(|reference| reference.rsplit('/').next())
        .map_or(schema, |name| &doc["components"]["schemas"][name]);
    for field in ["zone", "expected_revision", "mode", "source"] {
        assert!(
            schema["required"]
                .as_array()
                .unwrap()
                .contains(&json!(field))
        );
    }
    let response = &op["responses"]["200"]["content"]["application/json"]["schema"];
    let response = response["$ref"]
        .as_str()
        .and_then(|reference| reference.rsplit('/').next())
        .map_or(response, |name| &doc["components"]["schemas"][name]);
    assert_eq!(response["oneOf"].as_array().unwrap().len(), 2);
}
