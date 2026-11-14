use super::*;
use crate::tools::Daemon;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_proto::{ProtoError, Transport};
use serde_json::{Value, json};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

struct NoLocalIo;
impl Transport for NoLocalIo {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("forwarded, refused or malformed routine edits must not reach local speakers")
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
        Box::new(NoLocalIo),
        Box::new(|_| panic!("must not survey local speakers")),
        Policy::default(),
        Client::Unknown,
        Box::new(SystemClock),
    )
    .with_daemon(reply.clone())
}

fn replacement() -> UpdateScheduleRequest {
    request(
        7,
        "schedule-v1-observed".into(),
        json!({
            "when":"weekdays 08:00", "action":"dj_start", "zone":"Den@S2",
            "direction":{"constraints":{"include_artists":["Artist, with comma"]}, "for_secs":3601},
        }),
    )
    .unwrap()
}

#[test]
fn routine_update_forwards_exact_identity_revision_and_complete_direction_once() {
    let outcome = json!({
        "id":7, "revision":"schedule-v1-updated", "when":"weekdays 08:00",
        "action":"dj_start", "zone":"Den@S2",
        "direction":{"constraints":{"include_artists":["Artist, with comma"]}, "for_secs":3601},
        "summary":"start the DJ in Den", "creator":"owner", "enabled":false,
    });
    let reply = Arc::new(Reply {
        calls: Mutex::default(),
        answer: Some(Ok(outcome.clone())),
    });
    let result = backend(&reply).update_schedule(&replacement()).unwrap();
    assert!(!result.is_error);
    let updated: ScheduleDto = serde_json::from_value(result.structured_content.unwrap()).unwrap();
    let expected: ScheduleDto = serde_json::from_value(outcome).unwrap();
    assert_eq!(updated, expected);
    assert_eq!(
        *reply.calls.lock().unwrap(),
        [json!(["POST", "/schedules/update", replacement()])]
    );
}

#[test]
fn failed_or_malformed_update_replies_never_fall_back_to_a_second_store() {
    for answer in [
        Err(Failure::new(
            ErrorCode::ConfigChanged,
            "another editor changed the definition",
        )),
        Err(Failure::new(
            ErrorCode::OutcomeUncertain,
            "reply lost after commit",
        )),
        Err(Failure::new(
            ErrorCode::PolicyDenied,
            "owner cannot perform the new action",
        )),
        Err(Failure::new(ErrorCode::UnknownSchedule, "removed")),
        Ok(json!({"id":7, "when":"daily 08:00"})),
    ] {
        let reply = Arc::new(Reply {
            calls: Mutex::default(),
            answer: Some(answer),
        });
        assert!(backend(&reply).update_schedule(&replacement()).is_err());
        assert_eq!(
            *reply.calls.lock().unwrap(),
            [json!(["POST", "/schedules/update", replacement()])]
        );
    }
}

#[test]
fn invalid_proofs_and_action_fields_are_rejected_before_daemon_io() {
    for (id, revision, schedule) in [
        (
            0,
            "revision",
            json!({"when":"daily 08:00", "action":"pause", "zone":"Den"}),
        ),
        (
            7,
            " ",
            json!({"when":"daily 08:00", "action":"pause", "zone":"Den"}),
        ),
        (
            7,
            "revision",
            json!({"when":"daily 08:00", "action":"volume", "zone":"Den", "volume":101}),
        ),
        (
            7,
            "revision",
            json!({"when":"daily 08:00", "action":"pause", "zone":"Den", "creator":"admin"}),
        ),
        (7, "revision", json!({"action":"pause", "zone":"Den"})),
        (
            7,
            "revision",
            json!({"when":"daily 08:00", "action":"announce", "announcement":{"text":"Reminder", "chime":"bell"}}),
        ),
    ] {
        assert!(request(id, revision.into(), schedule).is_err());
    }
    let reply = Arc::new(Reply {
        calls: Mutex::default(),
        answer: None,
    });
    let mut invalid = replacement();
    invalid.expected_revision.clear();
    assert!(backend(&reply).update_schedule(&invalid).is_err());
    assert!(reply.calls.lock().unwrap().is_empty());
    assert!(
        backend(&reply)
            .update_schedule(&replacement())
            .unwrap_err()
            .message
            .contains("POLICY_DENIED")
    );
}

#[test]
fn update_accepts_all_scheduled_actions_without_discarding_their_fields() {
    for schedule in [
        json!({"when":"in 45m", "action":"pause", "zone":"all@S1", "fade_secs":0}),
        json!({"when":"daily 08:00", "action":"volume", "zone":"Morning rooms", "volume":0}),
        json!({"when":"weekends 09:00", "action":"apply_scene", "scene":"Brunch"}),
        json!({"when":"daily 12:00", "action":"announce", "announcement":{"text":"Tea time", "voice":"fixture", "rooms":["Den@S2"], "volume":0}}),
        json!({"when":"daily 12:00", "action":"announce", "announcement":{"chime":"bell"}}),
    ] {
        let expected: ScheduleRequest = serde_json::from_value(schedule.clone()).unwrap();
        assert_eq!(
            request(7, "revision".into(), schedule).unwrap().schedule,
            expected
        );
    }
}

#[test]
fn update_tool_requires_a_revision_and_definition_and_is_not_advertised_as_retryable() {
    let server = fastmcp_server::ServerBuilder::try_new_with_fixed_protocol_policy(
        "routine-edit-test",
        "1.0",
        fastmcp::ProtocolPolicy::Auto,
    )
    .unwrap()
    .tool(UpdateSchedule)
    .try_build()
    .unwrap();
    let tools = server.tools();
    let tool = tools.iter().find(|t| t.name == "update_schedule").unwrap();
    let value = serde_json::to_value(tool).unwrap();
    let required = value["inputSchema"]["required"].as_array().unwrap();
    for field in ["id", "expected_revision", "schedule"] {
        assert!(required.contains(&json!(field)));
    }
    assert_ne!(value["annotations"]["readOnlyHint"], true);
    assert_ne!(value["annotations"]["idempotentHint"], true);
}
