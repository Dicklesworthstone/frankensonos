use super::*;
use crate::tools::Daemon;
use fsonos_api::surface::queue::QueueAddRequest;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_proto::{ProtoError, Transport};
use serde_json::{Value, json};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

struct NoSpeakerIo;

impl Transport for NoSpeakerIo {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("a forwarded or denied queue add must not contact local speakers")
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
        Box::new(|_| panic!("queue-add forwarding must not survey local speakers")),
        Policy::default(),
        Client::Unknown,
        Box::new(SystemClock),
    )
    .with_daemon(reply.clone())
}

fn request() -> QueueRequest {
    QueueRequest {
        zone: "Den@S2".into(),
        favorite: Some("FV:2/7".into()),
        next: true,
        ..QueueRequest::default()
    }
}

fn outcome() -> Value {
    json!({"done": "queued native favorite", "zone": "Den", "first": 3,
        "count": 4, "playing_queue": true})
}

#[test]
fn a_native_favorite_is_forwarded_once_with_no_caller_metadata() {
    let reply = Arc::new(Reply {
        calls: Mutex::default(),
        answer: Some(Ok(outcome())),
    });
    let done = backend(&reply).queue_request(&request()).unwrap();
    assert!(!done.is_error);
    assert_eq!(done.structured_content, Some(outcome()));
    assert_eq!(
        *reply.calls.lock().unwrap(),
        [json!(["POST", "/queue", {"zone": "Den@S2", "favorite": "FV:2/7", "next": true}])]
    );
}

#[test]
fn the_original_spotify_rust_helper_uses_the_daemons_action_log() {
    let reply = Arc::new(Reply {
        calls: Mutex::default(),
        answer: Some(Ok(outcome())),
    });
    let req = QueueAddRequest {
        zone: "Den@S2".into(),
        source_uri: "spotify:album:0SimQueueAlbum000000001".into(),
        title: Some("Album".into()),
        next: false,
    };
    assert!(!backend(&reply).add_to_queue(&req).unwrap().is_error);
    assert_eq!(*reply.calls.lock().unwrap(), [json!(["POST", "/queue", req])]);
}

#[test]
fn failed_and_malformed_daemon_adds_never_replay_locally() {
    for answer in [
        Err(Failure::new(ErrorCode::PlayerUnreachable, "lost reply")),
        Err(Failure::new(ErrorCode::UpnpFault, "uncertain insertion")),
        Err(Failure::new(ErrorCode::PolicyDenied, "not allowed")),
        Ok(json!({"done": "missing confirmed range"})),
    ] {
        let reply = Arc::new(Reply {
            calls: Mutex::default(),
            answer: Some(answer),
        });
        assert!(backend(&reply).queue_request(&request()).is_err());
        assert_eq!(reply.calls.lock().unwrap().len(), 1);
    }
}

#[test]
fn no_daemon_still_means_local_policy_before_any_speaker_io() {
    let reply = Arc::new(Reply {
        calls: Mutex::default(),
        answer: None,
    });
    let error = backend(&reply).queue_request(&request()).unwrap_err();
    assert!(error.message.contains("POLICY_DENIED"));
    assert_eq!(reply.calls.lock().unwrap().len(), 1);
}

#[test]
fn an_ambiguous_source_choice_is_not_sent_to_the_daemon() {
    let reply = Arc::new(Reply {
        calls: Mutex::default(),
        answer: Some(Ok(outcome())),
    });
    for req in [
        QueueRequest {
            source_uri: Some("spotify:track:0SimQueueTrack000000001".into()),
            ..request()
        },
        QueueRequest {
            title: Some("ignored native metadata".into()),
            ..request()
        },
        QueueRequest {
            favorite: None,
            ..request()
        },
        QueueRequest {
            favorite: Some("".into()),
            ..request()
        },
    ] {
        assert!(backend(&reply).queue_request(&req).is_err());
    }
    assert!(reply.calls.lock().unwrap().is_empty());
}
