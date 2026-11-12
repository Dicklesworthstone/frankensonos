use super::*;
use crate::tools::Daemon;
use fsonos_api::surface::queue::MAX_QUEUE_TRACKS;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_proto::{ProtoError, Transport};
use serde_json::{Value, json};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

struct NoIo;

impl Transport for NoIo {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("a forwarded, invalid or denied selection must not reach local speakers")
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

fn request() -> QueueTracksRequest {
    let first = QueueTrackInput {
        source_uri: "spotify:track:4uLU6hMCjMI75M1A2tKUQC".into(),
        title: Some("Björk & strings".into()),
    };
    QueueTracksRequest {
        zone: "Member / ?@S2".into(),
        tracks: vec![
            first.clone(),
            QueueTrackInput {
                source_uri: "https://open.spotify.com/episode/512ojhOuo1ktJprKbVcKyQ?si=test"
                    .into(),
                title: None,
            },
            first,
        ],
        next: true,
        expected_revision: Some("q1:room&?#".into()),
    }
}

#[test]
fn selection_forwards_once_with_order_duplicates_titles_and_revision() {
    let expected = json!({
        "done": "queued 3 tracks next", "zone": "Lead", "first": 5,
        "count": 3, "playing_queue": true
    });
    let (backend, reply) = fixture(Some(Ok(expected.clone())));
    let result = backend.add_tracks_to_queue(&request()).unwrap();
    assert!(!result.is_error);
    assert_eq!(result.structured_content, Some(expected));
    assert_eq!(
        *reply.calls.lock().unwrap(),
        [json!(["POST", "/queue/tracks", request()])]
    );
}

#[test]
fn invalid_selection_fails_before_daemon_or_speaker_io() {
    let mut invalid = Vec::new();
    let mut empty = request();
    empty.tracks.clear();
    invalid.push(empty);
    let mut too_many = request();
    too_many.tracks = vec![too_many.tracks[0].clone(); MAX_QUEUE_TRACKS + 1];
    invalid.push(too_many);
    let mut container = request();
    container.tracks[1].source_uri = "spotify:album:4uLU6hMCjMI75M1A2tKUQC".into();
    invalid.push(container);
    let mut blank_revision = request();
    blank_revision.expected_revision = Some(" ".into());
    invalid.push(blank_revision);
    for request in invalid {
        let (backend, reply) = fixture(None);
        assert!(backend.add_tracks_to_queue(&request).is_err());
        assert!(reply.calls.lock().unwrap().is_empty());
    }
}

#[test]
fn failed_or_malformed_daemon_reply_never_replays_the_selection() {
    for answer in [
        Err(Failure::new(
            ErrorCode::QueueChanged,
            "the destination queue changed",
        )),
        Err(Failure::new(
            ErrorCode::PolicyDenied,
            "the daemon denies add_to_queue",
        )),
        Err(Failure::new(
            ErrorCode::OutcomeUncertain,
            "confirmation was lost",
        )),
        Err(Failure::new(
            ErrorCode::PlayerUnreachable,
            "the speaker did not reply",
        )),
        Ok(json!({"done": "missing confirmation fields"})),
    ] {
        let (backend, reply) = fixture(Some(answer));
        assert!(backend.add_tracks_to_queue(&request()).is_err());
        assert_eq!(
            *reply.calls.lock().unwrap(),
            [json!(["POST", "/queue/tracks", request()])]
        );
    }
}

#[test]
fn standalone_batches_require_the_existing_add_to_queue_permission() {
    let backend = Backend::new(
        Box::new(NoIo),
        Box::new(|_| panic!("a policy denial must precede discovery")),
        Policy::from_toml("[clients.\"mcp-stdio\"]\nallow = [\"add_tracks_to_queue\"]").unwrap(),
        Client::McpStdio,
        Box::new(SystemClock),
    );
    let failure = backend.add_tracks_to_queue(&request()).unwrap_err();
    assert_eq!(failure.data.unwrap()["code"], "POLICY_DENIED");
    assert!(failure.message.contains("add_to_queue"));
}

#[test]
fn tool_schema_describes_typed_selection_and_optional_destination_guard() {
    let definition = fastmcp_server::ToolHandler::definition(&AddTracksToQueue);
    assert_eq!(definition.name, "add_tracks_to_queue");
    assert_ne!(definition.annotations.and_then(|a| a.read_only), Some(true));
    let schema = definition.input_schema;
    assert_eq!(schema["required"], json!(["zone", "tracks"]));
    assert_eq!(schema["properties"]["tracks"]["type"], "array");
    let items = &schema["properties"]["tracks"]["items"];
    let items = items["$ref"]
        .as_str()
        .and_then(|reference| reference.strip_prefix('#'))
        .and_then(|pointer| schema.pointer(pointer))
        .unwrap_or(items);
    assert_eq!(items["properties"]["source_uri"]["type"], "string");
    assert!(items["properties"].get("title").is_some());
    assert_eq!(items["required"], json!(["source_uri"]));
    assert!(schema["properties"].get("expected_revision").is_some());
    let description = definition.description.unwrap();
    assert!(description.contains("1-10"));
    assert!(description.contains("duplicates"));
    assert!(description.contains("destination queue revision"));
    fastmcp_server::ServerBuilder::try_new_with_fixed_protocol_policy(
        "queue-tracks-test",
        "1.0",
        fastmcp::ProtocolPolicy::Auto,
    )
    .unwrap()
    .tool(AddTracksToQueue)
    .try_build()
    .expect("ordered selection has a valid schema in both MCP eras");
}
