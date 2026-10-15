//! Daemon selection is authoritative: no alternate reads or second writes.

use super::*;
use fsonos_core::clock::SystemClock;
use fsonos_proto::ProtoError;
use serde_json::{Value, json};
use std::net::IpAddr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

struct NoIo;

impl Transport for NoIo {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("a daemon reply must not reach standalone speaker I/O")
    }
}

struct Reply {
    answer: Option<Result<Value, Failure>>,
    calls: Mutex<Vec<Value>>,
}

impl Daemon for Reply {
    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Option<Result<Value, Failure>> {
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
        Box::new(|_| panic!("a daemon reply must not trigger a standalone survey")),
        Policy::default(),
        Client::Unknown,
        Box::new(SystemClock),
    )
    .with_daemon(reply.clone());
    (backend, reply)
}

fn text(result: &FinalCallToolResult) -> String {
    serde_json::to_value(result).unwrap()["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn control(backend: &Backend, path: &str, body: Value) -> McpResult<FinalCallToolResult> {
    match path {
        "/play" => backend.play(&serde_json::from_value(body).unwrap()),
        "/play/favorite" => backend.play_favorite(
            body["zone"].as_str().unwrap().into(),
            body["favorite"].as_str().unwrap().into(),
        ),
        "/pause" | "/resume" | "/next" | "/previous" => {
            let action = match path {
                "/pause" => TransportAction::Pause,
                "/resume" => TransportAction::Resume,
                "/next" => TransportAction::Next,
                _ => TransportAction::Previous,
            };
            backend.transport(&serde_json::from_value(body).unwrap(), action)
        }
        "/seek" => backend.seek(&serde_json::from_value(body).unwrap()),
        "/volume" => backend.set_volume(&serde_json::from_value(body).unwrap()),
        "/mute" => backend.mute(&serde_json::from_value(body).unwrap()),
        "/group" => backend.group(&serde_json::from_value(body).unwrap()),
        "/ungroup" => backend.ungroup(&serde_json::from_value(body).unwrap()),
        "/party" => backend.group_all(&serde_json::from_value(body).unwrap()),
        _ => panic!("unexpected test route"),
    }
}

fn writes() -> Vec<(&'static str, Value)> {
    vec![
        ("/play", json!({"zone":"Room@S2", "source_uri":"spotify:track:Exact", "title":"Title"})),
        ("/play/favorite", json!({"zone":"Room@S1", "favorite":"FV:2/7"})),
        ("/pause", json!({"zone":"everywhere"})),
        ("/resume", json!({"zone":"Room@S1"})),
        ("/next", json!({"zone":"downstairs"})),
        ("/previous", json!({"zone":"Room@S2"})),
        ("/seek", json!({"zone":"Room@S1", "by_secs":-15})),
        ("/volume", json!({"zone":"everywhere", "volume":null, "delta":5, "group":true})),
        ("/mute", json!({"zone":"Room@S2", "mute":false})),
        ("/group", json!({"zone":"Office@S1", "to":"Room@S1"})),
        ("/ungroup", json!({"zone":"all@S2"})),
        ("/party", json!({"zone":null, "household":"S1"})),
    ]
}

#[test]
fn ordinary_controls_forward_exact_requests_and_preserve_cap_notes() {
    let answer = json!({"done":"daemon acted", "changed":true,
        "notes":[{"code":"VOLUME_CLAMPED", "detail":"Room lowered first"}]});
    for (path, body) in writes() {
        let (backend, reply) = fixture(Some(Ok(answer.clone())));
        let result = control(&backend, path, body.clone()).unwrap();
        assert_eq!(result.structured_content, Some(answer.clone()), "{path}");
        assert!(text(&result).contains("Room lowered first"), "{path}");
        assert_eq!(*reply.calls.lock().unwrap(), [json!(["POST", path, body])]);
    }
}

#[test]
fn every_control_error_and_malformed_success_is_terminal() {
    for answer in [
        Err(Failure::new(ErrorCode::PlayerUnreachable, "reply lost")),
        Err(Failure::new(ErrorCode::NotReady, "DJ owns this queue")),
        Err(Failure::new(ErrorCode::PolicyDenied, "daemon denied")),
        Ok(json!({"not_an_outcome":true})),
    ] {
        for (path, body) in writes() {
            let (backend, reply) = fixture(Some(answer.clone()));
            assert!(control(&backend, path, body).is_err(), "{path}");
            assert_eq!(reply.calls.lock().unwrap().len(), 1, "{path}");
        }
    }
}

#[test]
fn standalone_controls_still_enforce_local_policy() {
    for (path, body) in writes() {
        let (backend, reply) = fixture(None);
        let error = control(&backend, path, body).unwrap_err();
        assert!(error.message.contains("POLICY_DENIED"), "{path}: {error:?}");
        assert_eq!(reply.calls.lock().unwrap().len(), 1);
    }
}

fn read(backend: &Backend, route: usize) -> McpResult<FinalCallToolResult> {
    match route {
        0 => backend.list_zones(),
        1 => backend.list_rooms(),
        2 => backend.zone("Room / &@S2"),
        3 => backend.zone_state("Room / &@S2"),
        4 => backend.queue("Room / &@S2", 7, 25),
        5 => backend.list_favorites("Room / &@S2"),
        6 => backend.search_library(&SearchRequest {
            query: "Björk & strings?".into(),
            zone: Some("Room / &@S2".into()),
            limit: Some(7),
        }),
        7 => backend.recent_plays(Some("Room / &@S2"), 15),
        8 => backend.recent_actions(&ActionsQuery {
            client: Some("person+tag@example.invalid".into()),
            since: Some(-3),
            limit: Some(5),
        }),
        9 => backend.dj_moods(Some("Room / &@S2")),
        _ => panic!("unexpected read"),
    }
}

#[test]
fn all_house_reads_use_the_daemon_and_never_mask_its_failures() {
    for route in 0..10 {
        for answer in [
            Err(Failure::new(ErrorCode::NotReady, "the daemon is discovering")),
            Ok(json!({"invalid_reply":true})),
        ] {
            let (backend, reply) = fixture(Some(answer));
            assert!(read(&backend, route).is_err(), "read {route}");
            let calls = reply.calls.lock().unwrap();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0][0], "GET");
            assert!(calls[0][2].is_null());
        }
    }
}

#[test]
fn list_responses_keep_mcp_envelopes_and_encode_query_values_once() {
    for (route, path, key) in [
        (0, "/zones", "zones"),
        (1, "/rooms", "rooms"),
        (5, "/favorites?zone=Room%20%2F%20%26%40S2", "favorites"),
        (6, "/library/search?q=Bj%C3%B6rk%20%26%20strings%3F&zone=Room%20%2F%20%26%40S2&limit=7", "hits"),
        (7, "/history?limit=15&zone=Room%20%2F%20%26%40S2", "plays"),
        (8, "/actions?limit=5&client=person%2Btag%40example.invalid&since=-3", "actions"),
    ] {
        let (backend, reply) = fixture(Some(Ok(json!([]))));
        let result = read(&backend, route).unwrap();
        assert_eq!(result.structured_content, Some(json!({(key):[]})));
        assert_eq!(*reply.calls.lock().unwrap(), [json!(["GET", path, null])]);
    }
}

#[test]
fn a_read_falls_back_only_when_no_daemon_was_selected() {
    let (backend, reply) = fixture(None);
    let calls = AtomicUsize::new(0);
    let answer: u32 = backend.read("/test", || {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(7)
    }).unwrap();
    assert_eq!(answer, 7);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(reply.calls.lock().unwrap().len(), 1);
}

#[test]
fn undo_uses_the_same_daemon_log_without_widening_ownership() {
    let answer = json!({"summary":"restored partly", "undone":7, "intent":"play",
        "failures":["Room: the queue changed"]});
    let (backend, reply) = fixture(Some(Ok(answer.clone())));
    let result = backend.undo_last().unwrap();
    assert_eq!(result.structured_content, Some(answer));
    assert_eq!(*reply.calls.lock().unwrap(), [json!(["POST", "/undo", {"own_only":true}])]);
}

#[test]
fn failed_or_malformed_undo_never_tries_the_local_action_log() {
    for answer in [
        Err(Failure::new(ErrorCode::PlayerUnreachable, "restore reply lost")),
        Ok(json!({"undone":7})),
    ] {
        let (backend, reply) = fixture(Some(answer));
        assert!(backend.undo_last().is_err());
        assert_eq!(reply.calls.lock().unwrap().len(), 1);
    }
}

#[test]
fn resource_reads_use_daemon_state_and_propagate_failure() {
    let (backend, reply) = fixture(Some(Ok(json!([]))));
    let document: Value = serde_json::from_str(&backend.zones_document().unwrap()).unwrap();
    assert_eq!(document, json!({"zones":[]}));
    assert_eq!(*reply.calls.lock().unwrap(), [json!(["GET", "/zones", null])]);
    let (backend, reply) = fixture(Some(Err(Failure::new(ErrorCode::NotReady, "not ready"))));
    assert!(backend.zone_document("Room / &@S2").is_err());
    assert_eq!(*reply.calls.lock().unwrap(), [json!(["GET", "/zones/Room%20%2F%20%26%40S2/state", null])]);
}
