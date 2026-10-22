use super::*;
use crate::tools::Daemon;
use fsonos_api::surface::queue::{QueueItemDto, QueueSearchDto};
use fsonos_api::{ErrorCode, Failure};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_proto::{ProtoError, Transport};
use serde_json::{Value, json};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

struct NoIo;

impl Transport for NoIo {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("a forwarded or invalid queue search must not contact local speakers")
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
        Client::McpStdio,
        Box::new(SystemClock),
    )
    .with_daemon(reply.clone());
    (backend, reply)
}

fn request() -> QueueSearchRequest {
    QueueSearchRequest {
        zone: "Member / ?@S2".into(),
        query: "Björk & strings".into(),
        offset: 100,
        limit: 2,
        expected_revision: Some("q1:room&?#".into()),
    }
}

fn result() -> QueueSearchDto {
    QueueSearchDto {
        zone: "Lead".into(),
        query: "Björk & strings".into(),
        total: 125,
        revision: "q1:room&?#".into(),
        offset: 100,
        examined: 11,
        next_offset: Some(111),
        complete: false,
        items: [103, 111]
            .map(|position| QueueItemDto {
                position,
                title: "Björk with strings".into(),
                creator: Some("Björk".into()),
                album: None,
                uri: Some("x-file-cifs://example.invalid/duplicate.flac".into()),
            })
            .into(),
    }
}

#[test]
fn a_search_preserves_exact_occurrences_and_continuation_through_the_daemon() {
    let expected = result();
    let (backend, reply) = fixture(Some(Ok(serde_json::to_value(&expected).unwrap())));
    let found = backend.search_queue(&request()).unwrap();
    assert_eq!(
        serde_json::from_value::<QueueSearchDto>(found.structured_content.unwrap()).unwrap(),
        expected
    );
    assert_eq!(
        *reply.calls.lock().unwrap(),
        [json!([
            "GET",
            "/zones/Member%20%2F%20%3F%40S2/queue/search?q=Bj%C3%B6rk%20%26%20strings&offset=100&limit=2&expected_revision=q1%3Aroom%26%3F%23",
            null
        ])]
    );
}

#[test]
fn invalid_requests_never_reach_the_selected_daemon_or_local_speakers() {
    let mut invalid = Vec::new();
    let mut req = request();
    req.expected_revision = None;
    invalid.push(req);
    let mut req = request();
    req.query = "?!".into();
    invalid.push(req);
    let mut req = request();
    req.limit = 0;
    invalid.push(req);
    let mut req = request();
    req.limit = 201;
    invalid.push(req);
    for req in invalid {
        let (backend, reply) = fixture(Some(Ok(serde_json::to_value(result()).unwrap())));
        let error = backend.search_queue(&req).unwrap_err();
        assert_eq!(error.data.unwrap()["code"], "INVALID_ARGUMENT");
        assert!(reply.calls.lock().unwrap().is_empty());
    }
}

#[test]
fn stale_queue_and_daemon_errors_do_not_restart_or_fall_back() {
    for answer in [
        Err(Failure::new(ErrorCode::QueueChanged, "the queue changed")),
        Err(Failure::new(
            ErrorCode::PolicyDenied,
            "the daemon denies get_queue",
        )),
        Err(Failure::new(
            ErrorCode::PlayerUnreachable,
            "the speaker did not reply",
        )),
        Ok(json!({"items": []})),
    ] {
        let (backend, reply) = fixture(Some(answer));
        assert!(backend.search_queue(&request()).is_err());
        assert_eq!(reply.calls.lock().unwrap().len(), 1);
    }
}

#[test]
fn standalone_search_uses_get_queue_permission_before_speaker_io() {
    let backend = Backend::new(
        Box::new(NoIo),
        Box::new(|_| panic!("a policy denial must precede discovery")),
        Policy::from_toml("[clients.\"mcp-stdio\"]\nallow = [\"search_queue\"]").unwrap(),
        Client::McpStdio,
        Box::new(SystemClock),
    );
    let failure = backend.search_queue(&request()).unwrap_err();
    assert_eq!(failure.data.unwrap()["code"], "POLICY_DENIED");
    assert!(failure.message.contains("get_queue"));
}

#[test]
fn search_tool_registers_on_the_dual_era_server() {
    fastmcp_server::ServerBuilder::try_new_with_fixed_protocol_policy(
        "queue-search-test",
        "1.0",
        fastmcp::ProtocolPolicy::Auto,
    )
    .unwrap()
    .tool(SearchQueue)
    .try_build()
    .expect("queue search has a valid tool schema");
}
