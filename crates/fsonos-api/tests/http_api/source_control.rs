//! The browser's reviewed source contract is enforced by the actual HTTP route.

use super::*;
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHousehold, SimModel, SimPlayerSpec};
use fsonos_types::TransportState;

#[test]
fn reviewed_source_http_rejects_stale_audiences_before_speaker_io() {
    const SOURCE: &str = "x-rincon-mp3radio://stream.example.invalid/reviewed.mp3";
    for s2 in [false, true] {
        let builder = SimHousehold::builder();
        let sim = if s2 {
            builder
                .s2(["Living Room", "Kitchen"].map(|name| SimPlayerSpec::new(name, SimModel::One)))
        } else {
            builder.s1(["Living Room", "Kitchen"]
                .map(|name| SimPlayerSpec::new(name, SimModel::Play5Gen1)))
        }
        .spawn()
        .unwrap();
        let lan = sim.lan();
        let lead = sim.player("Living Room").unwrap();
        soap::join_group(
            &lan,
            sim.player("Kitchen").unwrap().ip,
            &PlayerId(lead.uuid.clone()),
        )
        .unwrap();
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&lan, lead.ip).unwrap());
        let surface = Arc::new(Surface::new(
            Box::new(sim.lan()),
            Box::new(move |_| Ok(vec![house.clone()])),
            Policy::default(),
            Box::new(SystemClock),
        ));
        let web = WebPolicy::for_listener("127.0.0.1:0".parse().unwrap(), &[]);
        let app = fsonos_api::app(
            &surface,
            &fsonos_api::Identity::fixed(Client::LoopbackHttp),
            &web,
        );
        let api = Api::serve(app, Arc::new(Mutex::new(Vec::new())), &web);
        let (status, resolved) = api.get("/rooms/resolve?target=Kitchen");
        assert_eq!(status, 200, "{resolved}");
        let start = sim.soap_log().len();
        let request = json!({
            "zone": "Kitchen",
            "expected_revision": "0".repeat(64),
            "mode": "play",
            "source": {"kind": "uri", "source_uri": SOURCE},
        });
        let (status, failure) = api.post("/sources/control", &request);
        assert_eq!(status, 409, "{failure}");
        assert_eq!(failure["code"], "NOT_COORDINATOR");
        assert_eq!(sim.soap_log().len(), start);

        let mut request = request;
        request["expected_revision"] = resolved["revision"].clone();
        let (status, played) = api.post("/sources/control", &request);
        assert_eq!(status, 200, "{played}");
        assert_eq!(played["changed"], true);
        let calls = sim.soap_log();
        for action in ["SetAVTransportURI", "Play"] {
            let writes: Vec<_> = calls[start..]
                .iter()
                .filter(|call| call.action == action)
                .collect();
            assert_eq!(writes.len(), 1, "{action}: {writes:?}");
            assert_eq!(writes[0].room, "Living Room");
        }
        assert_eq!(soap::get_media_info(&lan, lead.ip).unwrap().uri, SOURCE);
        assert_eq!(
            soap::get_transport_info(&lan, lead.ip).unwrap().state,
            TransportState::Playing
        );
    }
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
    assert_eq!(op["operationId"], "control_source");
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
