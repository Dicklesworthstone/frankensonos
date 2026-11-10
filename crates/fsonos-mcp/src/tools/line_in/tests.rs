use super::*;
use crate::tools::Daemon;
use fsonos_api::{ErrorCode, Failure};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_proto::{ProtoError, Transport};
use serde_json::{Value, json};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

struct NoSpeakerIo;

impl Transport for NoSpeakerIo {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("forwarded or denied line-in requests must not reach local speakers")
    }
}

struct Reply {
    calls: Mutex<Vec<Value>>,
    answer: Option<Result<Value, Failure>>,
}

impl Daemon for Reply {
    fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Option<Result<Value, Failure>> {
        self.calls.lock().unwrap().push(json!([method, path, body]));
        self.answer.clone()
    }
}

fn backend(reply: &Arc<Reply>) -> Backend {
    Backend::new(
        Box::new(NoSpeakerIo),
        Box::new(|_| panic!("line-in forwarding must not survey local speakers")),
        Policy::default(),
        Client::Unknown,
        Box::new(SystemClock),
    )
    .with_daemon(reply.clone())
}

fn request() -> PlayLineInRequest {
    PlayLineInRequest {
        zone: "Den & Study@S2".into(),
        source: "RINCON_INPUT_RIGHT".into(),
    }
}

#[test]
fn source_reads_preserve_exact_players_and_partial_failures_from_the_selected_daemon() {
    let sources = json!({
        "sources":[{
            "player":"RINCON_INPUT_RIGHT", "room":"Music Room",
            "name":"Turntable", "icon":"Turntable"
        }],
        "unavailable":[{
            "player":"RINCON_OTHER", "room":"Other Room", "detail":"input description unavailable"
        }]
    });
    let reply = Arc::new(Reply {
        calls: Mutex::default(),
        answer: Some(Ok(sources.clone())),
    });
    let result = backend(&reply).list_line_in("Den & Study@S2").unwrap();
    assert!(!result.is_error);
    assert_eq!(result.structured_content, Some(sources));
    assert_eq!(
        *reply.calls.lock().unwrap(),
        [json!(["GET", "/line-in?zone=Den%20%26%20Study%40S2", null])]
    );
}

#[test]
fn failed_or_malformed_source_reads_never_query_a_different_household() {
    for answer in [
        Err(Failure::new(
            ErrorCode::PlayerUnreachable,
            "source unavailable",
        )),
        Err(Failure::new(
            ErrorCode::PolicyDenied,
            "input reads not allowed",
        )),
        Ok(json!({"unavailable":[]})),
        Ok(json!({"sources":[{"player":"RINCON_INPUT"}], "unavailable":[]})),
    ] {
        let reply = Arc::new(Reply {
            calls: Mutex::default(),
            answer: Some(answer),
        });
        assert!(backend(&reply).list_line_in("Den@S2").is_err());
        assert_eq!(reply.calls.lock().unwrap().len(), 1);
    }
}

#[test]
fn native_input_play_forwards_the_exact_source_and_destination_once() {
    let outcome = json!({"done":"playing Turntable in Den & Study", "changed":true});
    for source in ["RINCON_INPUT_RIGHT", "Music Room@S2", "turntable"] {
        let reply = Arc::new(Reply {
            calls: Mutex::default(),
            answer: Some(Ok(outcome.clone())),
        });
        let request = PlayLineInRequest {
            source: source.into(),
            ..request()
        };
        let result = backend(&reply).play_line_in(&request).unwrap();
        assert!(!result.is_error);
        assert_eq!(result.structured_content, Some(outcome.clone()));
        assert_eq!(
            *reply.calls.lock().unwrap(),
            [json!(["POST", "/line-in/play", request])]
        );
    }
}

#[test]
fn uncertain_refused_or_malformed_playback_never_replays_on_local_speakers() {
    for answer in [
        Err(Failure::new(
            ErrorCode::OutcomeUncertain,
            "playback reply lost",
        )),
        Err(Failure::new(
            ErrorCode::PlayerUnreachable,
            "source unavailable",
        )),
        Err(Failure::new(
            ErrorCode::PolicyDenied,
            "playback not allowed",
        )),
        Err(Failure::new(ErrorCode::NotReady, "destination changed")),
        Ok(json!({"done":"missing outcome"})),
    ] {
        let reply = Arc::new(Reply {
            calls: Mutex::default(),
            answer: Some(answer),
        });
        assert!(backend(&reply).play_line_in(&request()).is_err());
        assert_eq!(
            *reply.calls.lock().unwrap(),
            [json!(["POST", "/line-in/play", request()])]
        );
    }
}

#[test]
fn invalid_sources_are_not_sent_and_local_playback_requires_permission() {
    let reply = Arc::new(Reply {
        calls: Mutex::default(),
        answer: None,
    });
    for invalid in [
        PlayLineInRequest {
            zone: " ".into(),
            ..request()
        },
        PlayLineInRequest {
            source: " ".into(),
            ..request()
        },
        PlayLineInRequest {
            source: "bad\nsource".into(),
            ..request()
        },
    ] {
        assert!(backend(&reply).play_line_in(&invalid).is_err());
    }
    for zone in [
        " ".to_owned(),
        "x".repeat(fsonos_api::request::MAX_ZONE_LEN + 1),
    ] {
        assert!(backend(&reply).list_line_in(&zone).is_err());
    }
    assert!(reply.calls.lock().unwrap().is_empty());
    assert!(
        backend(&reply)
            .play_line_in(&request())
            .unwrap_err()
            .message
            .contains("POLICY_DENIED")
    );
    assert_eq!(reply.calls.lock().unwrap().len(), 1);
}

#[test]
fn line_in_tools_register_required_identity_and_read_write_annotations() {
    let server = fastmcp_server::ServerBuilder::try_new_with_fixed_protocol_policy(
        "line-in-test",
        "1.0",
        fastmcp::ProtocolPolicy::Auto,
    )
    .unwrap()
    .tool(ListLineIn)
    .tool(PlayLineIn)
    .try_build()
    .unwrap();
    let tools = server.tools();
    for (name, required, read_only) in [
        ("list_line_in", vec!["zone"], true),
        ("play_line_in", vec!["zone", "source"], false),
    ] {
        let tool = tools.iter().find(|tool| tool.name == name).unwrap();
        let value = serde_json::to_value(tool).unwrap();
        let schema = &value["inputSchema"];
        let declared = schema["required"].as_array().unwrap();
        assert_eq!(declared.len(), required.len(), "{value}");
        for field in &required {
            assert!(declared.contains(&json!(field)), "{value}");
        }
        assert_eq!(
            schema["properties"].as_object().unwrap().len(),
            required.len()
        );
        assert_eq!(value["annotations"]["readOnlyHint"] == true, read_only);
        assert_eq!(value["annotations"]["idempotentHint"] == true, read_only);
    }
}
