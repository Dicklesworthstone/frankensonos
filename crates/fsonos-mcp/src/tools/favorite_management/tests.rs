use super::*;
use crate::tools::Daemon;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_proto::{ProtoError, Transport};
use serde_json::{Value, json};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

struct NoSpeakerIo;
impl Transport for NoSpeakerIo {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("forwarded or denied favorite operations cannot reach local speakers")
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
        Box::new(|_| panic!("must not discover local speakers")),
        Policy::default(),
        Client::Unknown,
        Box::new(SystemClock),
    )
    .with_daemon(reply.clone())
}
fn save() -> SaveFavoriteRequest {
    SaveFavoriteRequest {
        zone: "Den@S2".into(),
        expected_revision: "current:a&b=2".into(),
        title: "Café & friends".into(),
    }
}
fn rename() -> RenameFavoriteRequest {
    RenameFavoriteRequest {
        zone: "Den@S2".into(),
        favorite: "FV:2/7".into(),
        expected_revision: "favorite:a&b=2".into(),
        title: "Café & friends".into(),
    }
}
fn remove() -> RemoveFavoriteRequest {
    RemoveFavoriteRequest {
        zone: "Den@S2".into(),
        favorite: "FV:2/7".into(),
        expected_revision: "favorite:a&b=2".into(),
    }
}
fn invoke(backend: &Backend, operation: &str) -> McpResult<FinalCallToolResult> {
    match operation {
        "save" => backend.save_favorite(&save()),
        "rename" => backend.rename_favorite(&rename()),
        _ => backend.remove_favorite(&remove()),
    }
}

#[test]
fn all_favorite_writes_keep_selected_daemon_identity_revision_and_receipt() {
    for operation in ["save", "rename", "remove"] {
        for logged in [false, true] {
            let receipt = json!({"done":"favorite change confirmed","zone":"Den","favorite":{"id":"FV:2/7","title":"Café & friends","kind":"track"},"operation":operation,"changed":true,"logged":logged});
            let reply = Arc::new(Reply {
                calls: Mutex::default(),
                answer: Some(Ok(receipt.clone())),
            });
            let result = invoke(&backend(&reply), operation).unwrap();
            assert!(!result.is_error);
            assert_eq!(result.structured_content, Some(receipt));
            let text = serde_json::to_string(&result.content).unwrap();
            assert_eq!(text.contains("action log could not be written"), !logged);
            let body = match operation {
                "save" => json!(save()),
                "rename" => json!(rename()),
                _ => json!(remove()),
            };
            assert_eq!(
                *reply.calls.lock().unwrap(),
                [json!(["POST", format!("/favorites/{operation}"), body])]
            );
        }
    }
}

#[test]
fn favorite_failures_and_malformed_acknowledgements_never_fall_back_or_replay() {
    for answer in [
        Err(Failure::new(ErrorCode::QueueChanged, "source changed")),
        Err(Failure::new(ErrorCode::OutcomeUncertain, "lost reply")),
        Err(Failure::new(ErrorCode::PlayerUnreachable, "unreachable")),
        Err(Failure::new(ErrorCode::PolicyDenied, "denied")),
        Err(Failure::new(ErrorCode::UpnpFault, "favorite full")),
        Ok(json!({"done":"missing required native receipt"})),
        Ok(
            json!({"done":"unknown operation","zone":"Den","favorite":{"id":"FV:2/7","title":"Song","kind":"track"},"operation":"overwrite","changed":true,"logged":true}),
        ),
    ] {
        for operation in ["save", "rename", "remove"] {
            let reply = Arc::new(Reply {
                calls: Mutex::default(),
                answer: Some(answer.clone()),
            });
            assert!(invoke(&backend(&reply), operation).is_err());
            assert_eq!(reply.calls.lock().unwrap().len(), 1);
        }
    }
}

#[test]
fn favorite_reads_forward_the_observed_household_and_native_id() {
    for current in [false, true] {
        let observed = json!({"zone":"Den & Study","favorite":{"id":if current {""} else {"FV:2/7"},"title":"Song","kind":"track"},"revision":"opaque"});
        let reply = Arc::new(Reply {
            calls: Mutex::default(),
            answer: Some(Ok(observed.clone())),
        });
        let backend = backend(&reply);
        let (result, path) = if current {
            (
                backend.current_favorite("Den & Study@S2"),
                "/favorites/current?zone=Den%20%26%20Study%40S2",
            )
        } else {
            (
                backend.inspect_favorite(&InspectFavoriteRequest {
                    zone: "Den & Study@S2".into(),
                    favorite: "FV:2/7".into(),
                }),
                "/favorites/inspect?zone=Den%20%26%20Study%40S2&favorite=FV%3A2%2F7",
            )
        };
        assert_eq!(result.unwrap().structured_content, Some(observed));
        assert_eq!(*reply.calls.lock().unwrap(), [json!(["GET", path, null])]);
    }
}

#[test]
fn invalid_requests_and_read_only_callers_cannot_reach_speakers() {
    let reply = Arc::new(Reply {
        calls: Mutex::default(),
        answer: None,
    });
    assert!(
        backend(&reply)
            .save_favorite(&SaveFavoriteRequest {
                expected_revision: "".into(),
                ..save()
            })
            .is_err()
    );
    assert!(
        backend(&reply)
            .rename_favorite(&RenameFavoriteRequest {
                favorite: "Song".into(),
                ..rename()
            })
            .is_err()
    );
    assert!(
        backend(&reply)
            .remove_favorite(&RemoveFavoriteRequest {
                favorite: "7".into(),
                ..remove()
            })
            .is_err()
    );
    assert!(reply.calls.lock().unwrap().is_empty());
    for operation in ["save", "rename", "remove"] {
        assert!(invoke(&backend(&reply), operation).is_err());
    }
}
