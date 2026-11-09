use super::*;
use crate::tools::Daemon;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_proto::{ProtoError, Transport};
use serde_json::{Value, json};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

struct NoIo;
impl Transport for NoIo {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("configuration forwarding must not touch a speaker")
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
        Box::new(|_| panic!("configuration forwarding must not survey")),
        Policy::default(),
        Client::Unknown,
        Box::new(SystemClock),
    )
    .with_daemon(reply.clone())
}

fn configuration() -> DjConfigDto {
    serde_json::from_value(json!({
        "revision":"a".repeat(64),
        "config":{"moods":{"study":{"periods":["baroque"],"song_keywords":["piano"],"expires_at":2000000000,"strict":true}},"programs":[]},
        "builtin_moods":{"calm":{"energy_bias":-1}},
        "builtin_programs":[],
        "moods":{"study":{"periods":["baroque"],"song_keywords":["piano"],"expires_at":2000000000,"strict":true},"calm":{"energy_bias":-1}},
        "programs":[]
    })).unwrap()
}

fn request() -> DjConfigureRequest {
    let configuration = configuration();
    DjConfigureRequest {
        expected_revision: configuration.revision,
        config: configuration.config,
    }
}

#[test]
fn configuration_read_preserves_the_revision_and_both_effective_and_editable_definitions() {
    let value = serde_json::to_value(configuration()).unwrap();
    let reply = Arc::new(Reply {
        calls: Mutex::default(),
        answer: Ok(value.clone()),
    });
    assert_eq!(
        backend(&reply)
            .dj_configuration()
            .unwrap()
            .structured_content,
        Some(value)
    );
    assert_eq!(
        *reply.calls.lock().unwrap(),
        [json!(["GET", "/dj/configuration", null])]
    );
}

#[test]
fn configure_forwards_every_field_and_exact_revision_once() {
    let saved = DjConfiguredDto {
        done: "Saved; future picks use the configuration".into(),
        changed: true,
        configuration: configuration(),
    };
    let value = serde_json::to_value(&saved).unwrap();
    let reply = Arc::new(Reply {
        calls: Mutex::default(),
        answer: Ok(value.clone()),
    });
    let request = request();
    let result = backend(&reply).dj_configure(&request).unwrap();
    assert_eq!(result.structured_content, Some(value));
    assert_eq!(
        *reply.calls.lock().unwrap(),
        [json!(["POST", "/dj/configuration", request])]
    );
}

#[test]
fn conflict_uncertainty_policy_failure_and_malformed_reply_never_replay() {
    for answer in [
        Err(Failure::new(ErrorCode::ConfigChanged, "changed").with_hint("Read current revision")),
        Err(Failure::new(ErrorCode::OutcomeUncertain, "reply lost")),
        Err(Failure::new(ErrorCode::PolicyDenied, "denied")),
        Ok(json!({"done":"missing revision and configuration"})),
    ] {
        let reply = Arc::new(Reply {
            calls: Mutex::default(),
            answer,
        });
        assert!(backend(&reply).dj_configure(&request()).is_err());
        assert_eq!(reply.calls.lock().unwrap().len(), 1);
    }
}

#[test]
fn malformed_revision_is_rejected_before_daemon_dispatch() {
    let reply = Arc::new(Reply {
        calls: Mutex::default(),
        answer: Ok(json!({})),
    });
    let mut request = request();
    request.expected_revision = "guessed".into();
    let error = backend(&reply).dj_configure(&request).unwrap_err();
    assert!(error.to_string().contains("expected_revision"));
    assert!(reply.calls.lock().unwrap().is_empty());
}

#[test]
fn full_profile_registers_typed_configuration_and_requires_both_write_arguments() {
    let server = crate::server_with(Vec::new(), crate::Profile::Full);
    let tools = server.tools();
    let read = tools
        .iter()
        .find(|tool| tool.name == "dj_configuration")
        .unwrap();
    assert_eq!(read.annotations.as_ref().unwrap().read_only, Some(true));
    assert_eq!(read.annotations.as_ref().unwrap().idempotent, Some(true));
    let write = tools
        .iter()
        .find(|tool| tool.name == "dj_configure")
        .unwrap();
    assert_ne!(
        write.annotations.as_ref().and_then(|a| a.read_only),
        Some(true)
    );
    assert_ne!(
        write.annotations.as_ref().and_then(|a| a.idempotent),
        Some(true)
    );
    let schema = &write.input_schema;
    let required = schema["required"].as_array().unwrap();
    assert!(required.contains(&json!("expected_revision")));
    assert!(required.contains(&json!("config")));
    // The tool uses the API's complete typed schema; none of these stored
    // fields may disappear through the narrower temporary-steering DTO.
    let encoded = serde_json::to_string(schema).unwrap();
    for name in [
        "moods",
        "programs",
        "song_keywords",
        "expires_at",
        "strict",
        "days",
        "from",
        "to",
        "energy_bias",
    ] {
        assert!(
            encoded.contains(&format!("\"{name}\"")),
            "missing schema field {name}"
        );
    }
    let essentials = crate::server_with(Vec::new(), crate::Profile::Essentials);
    assert!(
        !essentials
            .tools()
            .iter()
            .any(|tool| matches!(tool.name.as_str(), "dj_configuration" | "dj_configure"))
    );
}
