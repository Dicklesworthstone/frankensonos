//! Ordered scene definitions and incomplete playback retain their MCP contract.

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
        panic!("a forwarded or invalid scene must not contact local speakers")
    }
}

struct Reply {
    calls: Mutex<Vec<Value>>,
    answer: Result<Value, Failure>,
}

impl Daemon for Reply {
    fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Option<Result<Value, Failure>> {
        self.calls.lock().unwrap().push(json!([method, path, body]));
        Some(self.answer.clone())
    }
}

fn backend(answer: Result<Value, Failure>) -> (Backend, Arc<Reply>) {
    let reply = Arc::new(Reply {
        calls: Mutex::default(),
        answer,
    });
    let backend = Backend::new(
        Box::new(NoSpeakerIo),
        Box::new(|_| panic!("scene forwarding must not discover local speakers")),
        Policy::default(),
        Client::Unknown,
        Box::new(SystemClock),
    )
    .with_daemon(reply.clone());
    (backend, reply)
}

fn tracks() -> Value {
    json!([
        {"source_uri": "spotify:track:1000000000000000000001", "title": "Opening"},
        {"source_uri": "spotify:episode:1000000000000000000002", "title": "Interlude"},
        {"source_uri": "spotify:track:1000000000000000000001", "title": "Reprise"}
    ])
}

fn definition() -> Value {
    json!({
        "groups": [{"coordinator": "Kitchen@S1", "members": [],
            "source": {"kind": "tracks", "tracks": tracks()}, "playing": false}],
        "volumes": {"Kitchen@S1": 20}, "mutes": {}
    })
}

fn saved() -> Value {
    let mut saved = definition();
    saved["name"] = json!("Evening");
    saved["saved"] = json!(0);
    saved
}

#[test]
fn save_and_read_keep_exact_occurrences_titles_and_silent_intent() {
    let (backend, reply) = backend(Ok(saved()));
    let saved = backend.save_scene_from("Evening", &definition()).unwrap();
    let read = backend.get_scene("Evening").unwrap();
    for result in [&saved, &read] {
        assert!(!result.is_error);
        assert_eq!(
            result.structured_content.as_ref().unwrap()["groups"][0]["source"]["tracks"],
            tracks()
        );
        let scene: SceneDto =
            serde_json::from_value(result.structured_content.clone().unwrap()).unwrap();
        let text = scene_line(&scene);
        assert!(text.contains("3 selected tracks in order"), "{text}");
        assert!(text.find("1. Opening").unwrap() < text.find("2. Interlude").unwrap());
        assert!(text.find("2. Interlude").unwrap() < text.find("3. Reprise").unwrap());
        assert!(text.contains("replaces the queue"));
        assert!(text.contains("ready without playback"));
    }
    assert_eq!(
        *reply.calls.lock().unwrap(),
        [
            json!(["PUT", "/scenes/Evening", definition()]),
            json!(["GET", "/scenes/Evening", null])
        ]
    );
}

#[test]
fn invalid_ordered_sources_fail_before_daemon_or_speaker_io() {
    let (backend, reply) = backend(Ok(saved()));
    for source in [
        json!({"kind": "tracks"}),
        json!({"kind": "tracks", "tracks": []}),
        json!({"kind": "tracks", "tracks": vec![tracks()[0].clone(); 11]}),
        json!({"kind": "tracks", "tracks": [{"source_uri": "spotify:album:1000000000000000000001"}]}),
        json!({"kind": "tracks", "tracks": [{"source_uri": "https://stream.example.invalid/radio.mp3"}]}),
        json!({"kind": "tracks", "tracks": [{"source_uri": "spotify:track:bad"}]}),
        json!({"kind": "tracks", "tracks": tracks(), "uri": "spotify:track:1000000000000000000001"}),
        json!({"kind": "keep", "tracks": tracks()}),
    ] {
        let mut scene = definition();
        scene["groups"][0]["source"] = source;
        let error = backend.save_scene_from("Evening", &scene).unwrap_err();
        assert_eq!(error.data.unwrap()["code"], "INVALID_ARGUMENT");
    }
    assert!(reply.calls.lock().unwrap().is_empty());
}

fn applied(complete: bool, failed: bool) -> Value {
    let mut value = json!({
        "scene": "Evening", "done": "applied part of Evening", "changed": true,
        "complete": complete, "steps": ["Kitchen plays three selected tracks"]
    });
    if failed {
        value["failed"] = json!([{"step": "Den plays three selected tracks", "error": "speaker refused playback"}]);
    }
    value
}

#[test]
fn scene_apply_preserves_partial_or_contradictory_receipts_and_marks_tool_errors() {
    for (complete, failed) in [(true, false), (false, true), (true, true), (false, false)] {
        let receipt = applied(complete, failed);
        let (backend, reply) = backend(Ok(receipt.clone()));
        let result = backend
            .apply_scene("Evening", &SceneApplyRequest::default())
            .unwrap();
        assert_eq!(result.is_error, !complete || failed);
        assert_eq!(result.structured_content, Some(receipt));
        assert_eq!(reply.calls.lock().unwrap().len(), 1);
    }
}

#[test]
fn refused_lost_and_malformed_scene_responses_never_replay_locally() {
    for answer in [
        Err(Failure::new(
            ErrorCode::OutcomeUncertain,
            "lost after playback",
        )),
        Err(Failure::new(ErrorCode::PolicyDenied, "denied")),
        Ok(json!({"done": "missing complete receipt"})),
    ] {
        let (backend, reply) = backend(answer);
        assert!(
            backend
                .apply_scene("Evening", &SceneApplyRequest::default())
                .is_err()
        );
        assert_eq!(reply.calls.lock().unwrap().len(), 1);
    }
}

#[test]
fn scene_tool_descriptions_explain_authored_tracks_and_partial_delivery() {
    use fastmcp_server::ToolHandler;
    let save = SaveScene.definition();
    assert_eq!(save.input_schema["required"], json!(["name"]));
    let description = save.description.unwrap();
    for fragment in [
        "tracks",
        "source_uri",
        "1-10",
        "duplicates",
        "Nothing on the speakers changes",
    ] {
        assert!(description.contains(fragment), "{fragment}");
    }
    let apply = ApplyScene.definition().description.unwrap();
    assert!(apply.contains("tool-error flag"));
    assert!(apply.contains("silently"));
}
