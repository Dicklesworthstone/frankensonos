use super::*;
use crate::tools::Daemon;
use fsonos_api::surface::sonos_playlists::SonosPlaylistTrackMode;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_proto::{ProtoError, Transport};
use serde_json::{Value, json};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

struct NoIo;

impl Transport for NoIo {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("a forwarded, denied or invalid selection must not reach local speakers")
    }
}

struct Reply {
    answer: Option<Result<Value, Failure>>,
    calls: Mutex<Vec<Value>>,
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

fn fixture(answer: Option<Result<Value, Failure>>) -> (Backend, Arc<Reply>) {
    let reply = Arc::new(Reply {
        answer,
        calls: Mutex::default(),
    });
    let backend = Backend::new(
        Box::new(NoIo),
        Box::new(|_| panic!("a selected daemon must not discover local speakers")),
        Policy::default(),
        Client::Unknown,
        Box::new(SystemClock),
    )
    .with_daemon(reply.clone());
    (backend, reply)
}

fn request() -> QueueSelectionRequest {
    QueueSelectionRequest {
        zone: "Source / ?@S2".into(),
        target: Some("selected_s2".into()),
        positions: vec![3, 1, 3],
        expected_revision: "q1:source".into(),
        expected_target_revision: Some("a".repeat(64)),
        expected_queue_revision: None,
        mode: SonosPlaylistTrackMode::Append,
    }
}

fn receipt(complete: bool) -> Value {
    let failed = if complete {
        vec![]
    } else {
        vec![json!({
            "household": "S2", "coordinator": "original-source", "zone": "Source",
            "error": {
                "code": "OUTCOME_UNCERTAIN", "detail": "source destination did not confirm",
                "hint": "Read its queue before another command", "suggestions": [], "retryable": false
            }
        })]
    };
    json!({
        "done": "native queue selection returned", "source_zone": "Source / ?@S2",
        "zone": "selected_s2", "positions": [3, 1, 3], "titles": ["Third", "First", "Third"],
        "mode": "append", "changed": true, "notes": [],
        "queue_batch": {
            "done": "native queue delivery", "complete": complete, "logged": true,
            "undoable": complete,
            "added": [{
                "household": "S2", "coordinator": "destination", "queue": {
                    "done": "queued three occurrences", "zone": "Office",
                    "first": 4, "count": 3, "playing_queue": true
                }
            }],
            "failed": failed
        }
    })
}

#[test]
fn daemon_receipts_keep_order_and_partial_delivery_without_local_replay() {
    for complete in [true, false] {
        let expected = receipt(complete);
        let (backend, reply) = fixture(Some(Ok(expected.clone())));
        let result = backend.queue_selection(&request()).unwrap();
        assert_eq!(result.is_error, !complete);
        assert_eq!(result.structured_content, Some(expected));
        assert_eq!(
            *reply.calls.lock().unwrap(),
            [json!(["POST", "/queue/selection", request()])]
        );
    }
}

#[test]
fn failed_and_malformed_daemon_replies_are_terminal() {
    for answer in [
        Err(Failure::new(
            ErrorCode::QueueChanged,
            "the original queue changed",
        )),
        Err(Failure::new(
            ErrorCode::OutcomeUncertain,
            "confirmation was lost",
        )),
        Ok(json!({"done": "missing all confirmation fields"})),
    ] {
        let (backend, reply) = fixture(Some(answer));
        assert!(backend.queue_selection(&request()).is_err());
        assert_eq!(reply.calls.lock().unwrap().len(), 1);
    }
    let (backend, reply) = fixture(None);
    let mut invalid = request();
    invalid.positions = vec![1; 11];
    assert!(backend.queue_selection(&invalid).is_err());
    assert!(reply.calls.lock().unwrap().is_empty());
}

#[test]
fn native_queue_selection_keeps_the_existing_play_and_add_permissions() {
    for mode in [SonosPlaylistTrackMode::Play, SonosPlaylistTrackMode::Append] {
        let backend = Backend::new(
            Box::new(NoIo),
            Box::new(|_| panic!("permission must be checked before discovery")),
            Policy::from_toml("[clients.\"mcp-stdio\"]\nallow = [\"queue_selection\"]").unwrap(),
            Client::McpStdio,
            Box::new(SystemClock),
        );
        let mut request = request();
        request.mode = mode;
        let failure = backend.queue_selection(&request).unwrap_err();
        assert_eq!(failure.data.unwrap()["code"], "POLICY_DENIED");
    }
}

#[test]
fn native_queue_selection_is_a_full_profile_mutation_with_source_proof_fields() {
    let definition = fastmcp_server::ToolHandler::definition(&QueueSelection);
    assert_eq!(definition.name, "queue_selection");
    assert_ne!(definition.annotations.and_then(|a| a.read_only), Some(true));
    let schema = definition.input_schema;
    assert_eq!(
        schema["required"],
        json!(["zone", "positions", "expected_revision", "mode"])
    );
    assert!(schema["properties"].get("target").is_some());
    assert!(
        schema["properties"]
            .get("expected_target_revision")
            .is_some()
    );
    assert!(
        schema["properties"]
            .get("expected_queue_revision")
            .is_some()
    );
    let full = crate::server_with(Vec::new(), crate::Profile::Full);
    assert!(
        full.tools()
            .iter()
            .any(|tool| tool.name == "queue_selection")
    );
    let essentials = crate::server_with(Vec::new(), crate::Profile::Essentials);
    assert!(
        essentials
            .tools()
            .iter()
            .all(|tool| tool.name != "queue_selection")
    );
}
