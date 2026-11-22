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
        panic!("a forwarded or denied bedtime command must not contact local speakers")
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

fn backend(answer: Option<Result<Value, Failure>>) -> (Backend, Arc<Reply>) {
    let reply = Arc::new(Reply {
        calls: Mutex::default(),
        answer,
    });
    let backend = Backend::new(
        Box::new(NoSpeakerIo),
        Box::new(|_| panic!("bedtime forwarding must not discover local speakers")),
        Policy::default(),
        Client::Unknown,
        Box::new(SystemClock),
    )
    .with_daemon(reply.clone());
    (backend, reply)
}

fn request() -> SleepRequest {
    SleepRequest {
        zone: "downstairs".into(),
        duration: Some("45m".into()),
        expected_revision: Some("a".repeat(64)),
        ..SleepRequest::default()
    }
}

fn timer() -> Value {
    json!({
        "zone": "Kitchen", "ends": "2026-11-22T23:15:00Z", "remaining_secs": 2700,
        "fades": true, "fading": false, "done": "Kitchen pauses in 45m", "changed": true
    })
}

fn group_receipt(success: bool, failure: bool) -> Value {
    json!({
        "done": "updated selected sleep timers", "complete": !failure, "changed": success,
        "applied": if success { vec![json!({
            "household": "S1", "coordinator": "fixture-kitchen", "timer": timer()
        })] } else { vec![] },
        "failed": if failure { vec![json!({
            "household": "S2", "coordinator": "fixture-den", "zone": "Den",
            "error": {
                "code": "UPNP_FAULT", "detail": "speaker refused its timer",
                "hint": "Read the timer before another command.",
                "suggestions": [], "retryable": false
            }
        })] } else { vec![] }
    })
}

#[test]
fn single_and_room_set_timer_receipts_keep_every_result_and_signal_partial_delivery() {
    for (receipt, incomplete) in [
        (timer(), false),
        (group_receipt(true, false), false),
        (group_receipt(true, true), true),
        (group_receipt(false, true), true),
    ] {
        let (backend, reply) = backend(Some(Ok(receipt.clone())));
        let result = backend.set_sleep_timer(&request()).unwrap();
        assert_eq!(result.is_error, incomplete);
        assert_eq!(result.structured_content, Some(receipt));
        assert_eq!(
            *reply.calls.lock().unwrap(),
            [json!(["POST", "/sleep", request()])]
        );
    }
}

#[test]
fn contradictory_or_empty_timer_batches_cannot_report_complete_delivery() {
    let mut contradictory = group_receipt(true, true);
    contradictory["complete"] = json!(true);
    for receipt in [contradictory, group_receipt(false, false)] {
        let (backend, reply) = backend(Some(Ok(receipt.clone())));
        let result = backend.set_sleep_timer(&request()).unwrap();
        assert!(result.is_error);
        assert_eq!(result.structured_content, Some(receipt));
        assert_eq!(reply.calls.lock().unwrap().len(), 1);
    }
}

#[test]
fn lost_refused_and_malformed_timer_replies_never_fall_back_or_replay() {
    for receipt in [
        Err(Failure::new(ErrorCode::PlayerUnreachable, "lost reply")),
        Err(Failure::new(
            ErrorCode::OutcomeUncertain,
            "timer unconfirmed",
        )),
        Err(Failure::new(ErrorCode::PolicyDenied, "denied by daemon")),
        Ok(json!({"done": "missing timer results"})),
    ] {
        let (backend, reply) = backend(Some(receipt));
        assert!(backend.set_sleep_timer(&request()).is_err());
        assert_eq!(reply.calls.lock().unwrap().len(), 1);
    }
}

#[test]
fn standalone_timer_changes_still_require_permission_before_discovery() {
    let (backend, reply) = backend(None);
    let error = backend.set_sleep_timer(&request()).unwrap_err();
    assert_eq!(error.data.unwrap()["code"], "POLICY_DENIED");
    assert_eq!(reply.calls.lock().unwrap().len(), 1);
}

#[test]
fn invalid_sleep_changes_are_rejected_before_daemon_or_speaker_io() {
    let (backend, reply) = backend(None);
    let mut invalid = request();
    invalid.cancel = true;
    assert!(backend.set_sleep_timer(&invalid).is_err());
    invalid = request();
    invalid.zone.clear();
    assert!(backend.set_sleep_timer(&invalid).is_err());
    invalid = request();
    invalid.duration = Some("24h".into());
    assert!(backend.set_sleep_timer(&invalid).is_err());
    assert!(reply.calls.lock().unwrap().is_empty());
}

#[test]
fn fading_a_room_set_preserves_the_partial_receipt_and_reviewed_audience() {
    let outcome = json!({
        "done": "paused one group", "changed": true,
        "batch": {"complete": false, "results": [
            {"household": "S1", "target": "fixture-kitchen", "zone": "Kitchen",
             "result": {"status": "succeeded", "done": "faded and paused", "changed": true}},
            {"household": "S2", "target": "fixture-den", "zone": "Den",
             "result": {"status": "failed", "error": {
                "code": "UPNP_FAULT", "detail": "pause refused",
                "hint": "Read the group before another command.",
                "suggestions": [], "retryable": false
             }}}
        ]}
    });
    let (backend, reply) = backend(Some(Ok(outcome.clone())));
    let request = FadePauseRequest {
        zone: "all".into(),
        fade_secs: 30,
        expected_revision: Some("b".repeat(64)),
    };
    let result = backend.fade_and_pause(&request).unwrap();
    assert!(result.is_error);
    assert_eq!(result.structured_content, Some(outcome));
    assert_eq!(
        *reply.calls.lock().unwrap(),
        [json!(["POST", "/pause/fade", request])]
    );
}

#[test]
fn a_room_set_timer_read_is_forwarded_without_local_discovery() {
    let (backend, reply) = backend(Some(Ok(json!([timer()]))));
    let result = backend.list_sleep_timers(Some("all@S1")).unwrap();
    assert!(!result.is_error);
    assert_eq!(
        result.structured_content,
        Some(json!({"timers": [timer()]}))
    );
    assert_eq!(
        *reply.calls.lock().unwrap(),
        [json!(["GET", "/sleep?zone=all%40S1", null])]
    );
}

#[test]
fn bedtime_tool_schemas_expose_the_optional_audience_revision() {
    use fastmcp_server::ToolHandler;
    let sleep = SetSleepTimer.definition();
    let fade = FadeAndPause.definition();
    assert_eq!(sleep.input_schema["required"], json!(["zone"]));
    assert_eq!(fade.input_schema["required"], json!(["zone", "seconds"]));
    for definition in [sleep, fade] {
        assert!(
            definition.input_schema["properties"]
                .get("expected_revision")
                .is_some()
        );
        let description = definition.description.unwrap();
        assert!(description.contains("all@S1"));
        assert!(description.contains("all@S2"));
    }
    fastmcp_server::ServerBuilder::try_new_with_fixed_protocol_policy(
        "bedtime-tools-test",
        "1.0",
        fastmcp::ProtocolPolicy::Auto,
    )
    .unwrap()
    .tool(SetSleepTimer)
    .tool(FadeAndPause)
    .try_build()
    .expect("bedtime tools have valid schemas in both MCP eras");
}
