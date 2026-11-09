use super::*;
use crate::tools::Daemon;
use fsonos_api::{ErrorCode, Failure};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_proto::{ProtoError, Transport};
use serde_json::{Value, json};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

const ID: &str = "0123456789abcdef0123456789abcdef";

struct NoIo;
impl Transport for NoIo {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("daemon announcement jobs must never fall back to local speaker I/O")
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

fn backend(reply: &Arc<Reply>) -> Backend {
    Backend::new(
        Box::new(NoIo),
        Box::new(|_| panic!("forwarding must not survey")),
        Policy::default(),
        Client::Unknown,
        Box::new(SystemClock),
    )
    .with_daemon(reply.clone())
}

fn job() -> Value {
    json!({"id":ID,"done":"announcement queued","kind":"chime","rooms":["Den@S2"],
        "phase":"queued","created_at":10,"finished_at":null,"cancel_requested":false,
        "can_cancel":true,"progress":[],"result":null,"error":null})
}

#[test]
fn start_forwards_once_with_original_target_and_returns_the_retained_job() {
    let reply = Arc::new(Reply {
        calls: Mutex::default(),
        answer: Ok(job()),
    });
    let request = AnnounceRequest {
        chime: Some("bell".into()),
        rooms: vec!["Den@S2".into()],
        volume: Some(25),
        ..AnnounceRequest::default()
    };
    let result = backend(&reply).announce_start(&request).unwrap();
    assert_eq!(result.structured_content, Some(job()));
    assert_eq!(
        *reply.calls.lock().unwrap(),
        [json!(["POST", "/announcements", request])]
    );
}

#[test]
fn list_status_and_cancel_keep_the_same_daemon_job_identity() {
    let reply = Arc::new(Reply {
        calls: Mutex::default(),
        answer: Ok(json!([job()])),
    });
    assert_eq!(
        backend(&reply)
            .list_announcements()
            .unwrap()
            .structured_content,
        Some(json!({"announcements":[job()]}))
    );
    assert_eq!(
        *reply.calls.lock().unwrap(),
        [json!(["GET", "/announcements", null])]
    );
    for cancel in [false, true] {
        let reply = Arc::new(Reply {
            calls: Mutex::default(),
            answer: Ok(job()),
        });
        let backend = backend(&reply);
        let response = if cancel {
            backend.cancel_announcement(ID)
        } else {
            backend.get_announcement(ID)
        }
        .unwrap();
        assert_eq!(response.structured_content, Some(job()));
        assert_eq!(
            *reply.calls.lock().unwrap(),
            [if cancel {
                json!(["POST", format!("/announcements/{ID}/cancel"), {}])
            } else {
                json!(["GET", format!("/announcements/{ID}"), null])
            }]
        );
    }
}

#[test]
fn failed_or_malformed_start_and_cancel_never_replay_locally() {
    for answer in [
        Err(Failure::new(ErrorCode::OutcomeUncertain, "reply lost")),
        Err(Failure::new(ErrorCode::PolicyDenied, "not allowed")),
        Ok(json!({"done":"missing job identity"})),
    ] {
        for cancel in [false, true] {
            let reply = Arc::new(Reply {
                calls: Mutex::default(),
                answer: answer.clone(),
            });
            let backend = backend(&reply);
            let result = if cancel {
                backend.cancel_announcement(ID)
            } else {
                backend.announce_start(&AnnounceRequest {
                    chime: Some("beep".into()),
                    ..AnnounceRequest::default()
                })
            };
            assert!(result.is_err());
            assert_eq!(reply.calls.lock().unwrap().len(), 1);
        }
    }
}

#[test]
fn job_tools_register_with_read_hints_and_required_cancel_identity() {
    let server = fastmcp_server::ServerBuilder::try_new_with_fixed_protocol_policy(
        "announcement-jobs",
        "1.0",
        fastmcp::ProtocolPolicy::Auto,
    )
    .unwrap()
    .tool(AnnounceStart)
    .tool(ListAnnouncements)
    .tool(GetAnnouncement)
    .tool(CancelAnnouncement)
    .try_build()
    .unwrap();
    for tool in server.tools() {
        let value = serde_json::to_value(&tool).unwrap();
        let read = matches!(
            tool.name.as_str(),
            "list_announcements" | "get_announcement"
        );
        assert_eq!(value["annotations"]["readOnlyHint"] == true, read);
        if matches!(
            tool.name.as_str(),
            "get_announcement" | "cancel_announcement"
        ) {
            assert!(
                value["inputSchema"]["required"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("id"))
            );
        }
        if tool.name == "announce_start" {
            for key in ["text", "chime", "wav_base64", "voice", "rooms", "volume"] {
                assert!(value["inputSchema"]["properties"].get(key).is_some());
            }
        }
    }
}
