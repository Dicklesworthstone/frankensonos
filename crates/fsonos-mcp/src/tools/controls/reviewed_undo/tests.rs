use super::*;
use crate::tools::Daemon;
use fsonos_core::clock::{Clock, FakeClock};
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::{Action, MemStore, Store};
use fsonos_proto::{ProtoError, Transport};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::net::IpAddr;
use std::sync::{Arc, Barrier, Mutex};

struct NoLan;

impl Transport for NoLan {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("empty-snapshot undo tests must not touch speakers")
    }
}

fn local(client: Client) -> Backend {
    let clock = FakeClock::new("2026-10-15T12:00:00+00:00".parse().unwrap());
    let mut store = MemStore::default();
    for key in [client.key(), client.key(), "other-agent"] {
        store
            .record_action(&Action {
                at: clock.now().timestamp(),
                client: key.into(),
                surface: "mcp".into(),
                intent: "synthetic action".into(),
                decision: "allow".into(),
                result: "done".into(),
                before_state: Some("[]".into()),
                undo_of: None,
            })
            .unwrap();
    }
    let surface = fsonos_api::Surface::new(
        Box::new(NoLan),
        Box::new(|_| Ok(Vec::new())),
        Policy::default(),
        Box::new(clock),
    )
    .with_action_log(Box::new(store), "mcp");
    Backend::shared(Arc::new(surface), client)
}

type Reply = Option<Result<Value, Failure>>;
type Call = (String, String, Option<Value>);

struct Remote {
    replies: Mutex<VecDeque<Reply>>,
    calls: Mutex<Vec<Call>>,
}

impl Daemon for Remote {
    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Reply {
        self.calls
            .lock()
            .unwrap()
            .push((method.into(), path.into(), body.cloned()));
        self.replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("no unplanned daemon calls")
    }
}

fn remote(replies: Vec<Reply>) -> Arc<Remote> {
    Arc::new(Remote {
        replies: Mutex::new(replies.into()),
        calls: Mutex::new(Vec::new()),
    })
}

fn action(id: i64) -> Value {
    json!({
        "id": id, "at": 123, "client": "mcp-stdio", "surface": "mcp",
        "intent": "set_volume", "decision": "allow", "result": "done", "undoable": true
    })
}

fn preview_id(backend: &Backend) -> Value {
    backend.preview_undo().unwrap().structured_content.unwrap()["action"]["id"].clone()
}

#[test]
fn previews_and_consumes_only_its_own_exact_latest_action() {
    let backend = local(Client::McpStdio);
    assert_eq!(preview_id(&backend), 2);
    assert!(
        backend.undo_checked(3).is_err(),
        "someone else's newest is never selected"
    );
    assert!(
        backend.undo_checked(1).is_err(),
        "a superseded action is never selected"
    );
    let answer = backend.undo_checked(2).unwrap().structured_content.unwrap();
    assert_eq!(answer["undone"], 2);
    assert!(
        backend.undo_checked(2).is_err(),
        "repeat cannot consume the older action"
    );
    assert_eq!(preview_id(&backend), 1);
    backend.undo_checked(1).unwrap();
    assert!(preview_id(&backend).is_null());
}

#[test]
fn even_cli_identities_cannot_widen_a_checked_undo_to_other_clients() {
    let backend = local(Client::Cli);
    assert!(backend.undo_checked(3).is_err());
    assert_eq!(
        backend.undo_checked(2).unwrap().structured_content.unwrap()["undone"],
        2
    );
}

#[test]
fn preview_does_not_grant_mutation_permission() {
    let backend = local(Client::Unknown);
    backend.preview_undo().unwrap();
    let error = backend.undo_checked(2).unwrap_err();
    assert_eq!(error.data.unwrap()["code"], "POLICY_DENIED");
    assert_eq!(preview_id(&backend), 2);
}

#[test]
fn concurrent_confirmations_cannot_consume_the_next_action() {
    let backend = Arc::new(local(Client::McpStdio));
    let barrier = Arc::new(Barrier::new(2));
    let results: Vec<_> = std::thread::scope(|scope| {
        let jobs: Vec<_> = (0..2)
            .map(|_| {
                let backend = &backend;
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    backend.undo_checked(2).is_ok()
                })
            })
            .collect();
        jobs.into_iter().map(|job| job.join().unwrap()).collect()
    });
    assert_eq!(results.iter().filter(|ok| **ok).count(), 1);
    assert_eq!(preview_id(&backend), 1);
}

#[test]
fn daemon_preview_and_confirmation_use_exact_routes_and_only_expected_id() {
    let daemon = remote(vec![
        Some(Ok(action(7))),
        Some(Ok(json!({
            "summary": "partial restore", "undone": 7, "intent": "set_volume",
            "failures": ["Den: position unavailable"]
        }))),
    ]);
    let backend = local(Client::McpStdio).with_daemon(Arc::clone(&daemon) as Arc<dyn Daemon>);
    assert_eq!(preview_id(&backend), 7);
    let answer = backend.undo_checked(7).unwrap();
    assert_eq!(
        answer.structured_content.as_ref().unwrap()["failures"][0],
        "Den: position unavailable"
    );
    assert!(
        serde_json::to_value(&answer).unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Not restored: Den: position unavailable")
    );
    assert_eq!(
        *daemon.calls.lock().unwrap(),
        [
            ("GET".into(), "/undo/preview".into(), None),
            (
                "POST".into(),
                "/undo/checked".into(),
                Some(json!({ "expected_id": 7 })),
            ),
        ]
    );
    assert_eq!(
        backend
            .surface
            .preview_undo(&Client::McpStdio)
            .unwrap()
            .unwrap()
            .id,
        2
    );
}

#[test]
fn null_preview_is_not_replaced_by_local_history() {
    let daemon = remote(vec![Some(Ok(Value::Null))]);
    let backend = local(Client::McpStdio).with_daemon(daemon);
    assert!(backend.preview_undo().unwrap().structured_content.unwrap()["action"].is_null());
}

#[test]
fn disappearing_daemon_cannot_undo_a_matching_id_in_the_local_log() {
    let daemon = remote(vec![Some(Ok(action(2))), None]);
    let backend = local(Client::McpStdio).with_daemon(Arc::clone(&daemon) as Arc<dyn Daemon>);
    backend.preview_undo().unwrap();
    assert!(backend.undo_checked(2).is_err());
    assert_eq!(
        backend
            .surface
            .preview_undo(&Client::McpStdio)
            .unwrap()
            .unwrap()
            .id,
        2
    );
    assert_eq!(daemon.calls.lock().unwrap().len(), 2);
}

#[test]
fn invalid_ids_are_refused_before_any_daemon_request() {
    let daemon = remote(vec![]);
    let backend = local(Client::McpStdio).with_daemon(Arc::clone(&daemon) as Arc<dyn Daemon>);
    for id in [0, -1, i64::MIN] {
        assert_eq!(
            backend.undo_checked(id).unwrap_err().data.unwrap()["code"],
            "INVALID_ARGUMENT"
        );
    }
    assert!(daemon.calls.lock().unwrap().is_empty());
}

#[test]
fn malformed_or_foreign_previews_are_not_offered_for_confirmation() {
    let mut foreign = action(3);
    foreign["client"] = "foreign".into();
    let mut unundoable = action(3);
    unundoable["undoable"] = false.into();
    let mut consumed = action(3);
    consumed["undo_of"] = 2.into();
    for value in [json!({}), action(0), foreign, unundoable, consumed] {
        let daemon = remote(vec![Some(Ok(value))]);
        let backend = local(Client::McpStdio).with_daemon(daemon);
        assert!(backend.preview_undo().is_err());
    }
}

#[test]
fn failed_and_contradictory_confirmations_never_fall_back_or_retry() {
    for answer in [
        Err(Failure::invalid("target changed")),
        Err(Failure::new(ErrorCode::PlayerUnreachable, "lost reply")),
        Ok(json!({ "summary": "nothing to undo" })),
        Ok(json!({ "summary": "wrong action", "undone": 8, "intent": "other" })),
        Ok(json!({ "bad": true })),
    ] {
        let daemon = remote(vec![Some(answer)]);
        let backend = local(Client::McpStdio).with_daemon(Arc::clone(&daemon) as Arc<dyn Daemon>);
        assert!(backend.undo_checked(2).is_err());
        assert_eq!(daemon.calls.lock().unwrap().len(), 1);
        assert_eq!(
            backend
                .surface
                .preview_undo(&Client::McpStdio)
                .unwrap()
                .unwrap()
                .id,
            2
        );
    }
}
