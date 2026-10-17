use super::*;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_proto::{ProtoError, Transport};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

struct NoIo;

impl Transport for NoIo {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("a daemon resource must not contact the local speaker LAN")
    }
}

type Reply = (String, Option<Result<Value, Failure>>);

struct Script(Mutex<VecDeque<Reply>>);

impl Daemon for Script {
    fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Option<Result<Value, Failure>> {
        assert_eq!(method, "GET");
        assert!(body.is_none());
        let (expected, answer) = self.0.lock().unwrap().pop_front().expect("unexpected read");
        assert_eq!(path, expected);
        answer
    }
}

fn replies() -> Vec<Reply> {
    let zones = json!([
        {"coordinator_room":"Room / &", "members":["Room / &"],
         "transport_state":"playing", "household":"S1"},
        {"coordinator_room":"Room / &", "members":["Room / &"],
         "transport_state":"paused", "household":"S2"}
    ]);
    [
        ("/zones", zones),
        (
            "/dj/moods",
            json!({"moods":[], "programs":[], "now":{"source":"none"}}),
        ),
        (
            "/zones/Room%20%2F%20%26%40S1/dj",
            json!({
                "zone":"Room / &@S1", "running":true, "steering":{"source":"none"}
            }),
        ),
        (
            "/zones/Room%20%2F%20%26%40S2/dj",
            json!({
                "zone":"Room / &@S2", "running":false, "steering":{"source":"none"}
            }),
        ),
    ]
    .into_iter()
    .map(|(path, reply)| (path.into(), Some(Ok(reply))))
    .collect()
}

fn fixture(replies: Vec<Reply>) -> (Backend, Arc<Script>, Arc<AtomicUsize>) {
    let local = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&local);
    let script = Arc::new(Script(Mutex::new(replies.into())));
    let backend = Backend::new(
        Box::new(NoIo),
        Box::new(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
            Err(Failure::new(ErrorCode::NotReady, "local model unavailable"))
        }),
        Policy::default(),
        Client::McpStdio,
        Box::new(SystemClock),
    )
    .with_daemon(script.clone());
    (backend, script, local)
}

#[test]
fn off_lan_dj_resource_uses_daemon_topology_and_qualified_rooms() {
    let (backend, script, local) = fixture(replies());
    let document: Value = serde_json::from_str(&backend.dj_document().unwrap()).unwrap();
    assert_eq!(document["zones"][0]["zone"], "Room / &@S1");
    assert_eq!(document["zones"][0]["running"], true);
    assert_eq!(document["zones"][1]["zone"], "Room / &@S2");
    assert_eq!(document["zones"][1]["running"], false);
    assert_eq!(document["moods"]["now"]["source"], "none");
    assert!(script.0.lock().unwrap().is_empty());
    assert_eq!(local.load(Ordering::SeqCst), 0);
}

#[test]
fn failed_or_malformed_daemon_reads_never_fall_back_or_return_a_partial_document() {
    for step in 0..4 {
        for answer in [
            Err(Failure::new(
                ErrorCode::PolicyDenied,
                "remote policy denied",
            )),
            Ok(json!({"not_the_expected_document":true})),
        ] {
            let mut replies = replies();
            replies[step].1 = Some(answer);
            replies.truncate(step + 1);
            let (backend, script, local) = fixture(replies);
            assert!(backend.dj_document().is_err(), "step {step}");
            assert!(script.0.lock().unwrap().is_empty());
            assert_eq!(local.load(Ordering::SeqCst), 0, "step {step}");
        }
    }
}

#[test]
fn a_selected_daemon_disappearing_mid_document_is_terminal() {
    for step in 1..4 {
        let mut replies = replies();
        replies[step].1 = None;
        replies.truncate(step + 1);
        let (backend, script, local) = fixture(replies);
        let failure = backend.dj_document().unwrap_err();
        assert_eq!(failure.code, ErrorCode::NotReady);
        assert!(failure.detail.contains("daemon stopped answering"));
        assert!(script.0.lock().unwrap().is_empty());
        assert_eq!(local.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn no_daemon_selected_still_uses_the_standalone_model() {
    let (backend, script, local) = fixture(vec![("/zones".into(), None)]);
    assert_eq!(
        backend.dj_document().unwrap_err().detail,
        "local model unavailable"
    );
    assert_eq!(local.load(Ordering::SeqCst), 1);
    assert!(script.0.lock().unwrap().is_empty());
}
