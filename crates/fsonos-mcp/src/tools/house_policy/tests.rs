use super::*;
use crate::tools::Daemon;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_proto::{ProtoError, Transport};
use serde_json::{Value, json};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

struct NoLocalIo;
impl Transport for NoLocalIo {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("policy configuration never controls speakers")
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
        Box::new(NoLocalIo),
        Box::new(|_| panic!("configuration must not survey local speakers")),
        Policy::default(),
        Client::Unknown,
        Box::new(SystemClock),
    )
    .with_daemon(reply.clone())
}

fn configuration() -> Value {
    json!({
        "revision":"policy-v1-observed",
        "defaults":{"max_volume":70,"max_step":20,"fade_secs":0},
        "quiet_hours":{"start":"22:00","end":"07:00","max_volume":25},
        "rooms":[{"room":"Offline Bedroom","max_volume":40}],
        "clients":[{"client":"tag:fixture","allow":[],"deny":["volume"],
                    "capped":true,"mcp_profile":"essentials"}]
    })
}

fn changes() -> [PolicyConfigurationChange; 7] {
    [
        PolicyConfigurationChange::SetDefaults {
            max_volume: 60,
            max_step: 10,
            fade_secs: 5,
        },
        PolicyConfigurationChange::SetQuietHours {
            start: "22:00".into(),
            end: "07:00".into(),
            max_volume: 20,
        },
        PolicyConfigurationChange::ClearQuietHours {},
        PolicyConfigurationChange::SetRoom {
            room: "Offline Bedroom".into(),
            max_volume: Some(40),
            max_step: None,
        },
        PolicyConfigurationChange::RemoveRoom {
            room: "Offline Bedroom".into(),
        },
        PolicyConfigurationChange::SetClient {
            client: "tag:fixture".into(),
            allow: Some(vec![]),
            deny: vec!["volume".into()],
            grant: vec![],
            capped: Some(true),
            manage_others: false,
            mcp_profile: Some(fsonos_core::policy::McpProfile::Essentials),
        },
        PolicyConfigurationChange::RemoveClient {
            client: "tag:fixture".into(),
        },
    ]
}

fn request(change: PolicyConfigurationChange) -> PolicyConfigureRequest {
    PolicyConfigureRequest {
        expected_revision: "policy-v1-observed".into(),
        change,
    }
}

#[test]
fn read_preserves_the_complete_remote_configuration_without_local_inventory() {
    let reply = Arc::new(Reply {
        calls: Mutex::default(),
        answer: Ok(configuration()),
    });
    let result = backend(&reply).policy_configuration().unwrap();
    assert!(!result.is_error);
    assert_eq!(result.structured_content.unwrap(), configuration());
    assert_eq!(
        *reply.calls.lock().unwrap(),
        [json!(["GET", "/policy/configuration", null])]
    );
}

#[test]
fn all_seven_mutations_forward_one_exact_revision_and_complete_rule() {
    for change in changes() {
        let expected = request(change);
        let response = json!({"done":"Saved policy configuration", "changed":true, "configuration":configuration()});
        let reply = Arc::new(Reply {
            calls: Mutex::default(),
            answer: Ok(response.clone()),
        });
        let result = backend(&reply).configure_policy(&expected).unwrap();
        assert!(!result.is_error);
        assert_eq!(result.structured_content.unwrap(), response);
        assert_eq!(
            *reply.calls.lock().unwrap(),
            [json!(["POST", "/policy/configuration", expected])]
        );
    }
}

#[test]
fn denied_conflicting_uncertain_and_malformed_remote_replies_never_replay_locally() {
    for answer in [
        Err(Failure::new(
            ErrorCode::ConfigChanged,
            "an editor changed the file",
        )),
        Err(Failure::new(
            ErrorCode::OutcomeUncertain,
            "durability could not be confirmed",
        )),
        Err(Failure::new(
            ErrorCode::PolicyDenied,
            "only the owner or administrator may edit",
        )),
        Err(Failure::new(ErrorCode::NotImplemented, "older daemon")),
        Ok(json!({"done":"saved"})),
    ] {
        let reply = Arc::new(Reply {
            calls: Mutex::default(),
            answer,
        });
        assert!(
            backend(&reply)
                .configure_policy(&request(changes()[0].clone()))
                .is_err()
        );
        assert_eq!(reply.calls.lock().unwrap().len(), 1);
    }
}

#[test]
fn invalid_names_proofs_and_unknown_change_fields_are_rejected_before_dispatch() {
    let reply = Arc::new(Reply {
        calls: Mutex::default(),
        answer: Ok(configuration()),
    });
    for mut change in [
        request(PolicyConfigurationChange::SetDefaults {
            max_volume: 101,
            max_step: 20,
            fade_secs: 0,
        }),
        request(PolicyConfigurationChange::SetQuietHours {
            start: "22:00".into(),
            end: "22:00".into(),
            max_volume: 20,
        }),
        request(PolicyConfigurationChange::RemoveClient {
            client: "\n".into(),
        }),
    ] {
        assert!(backend(&reply).configure_policy(&change).is_err());
        change.expected_revision.clear();
        assert!(backend(&reply).configure_policy(&change).is_err());
    }
    let mut blank = request(changes()[0].clone());
    blank.expected_revision = " \n".into();
    assert!(backend(&reply).configure_policy(&blank).is_err());
    for value in [
        json!({"kind":"set_defaults","max_volume":40,"max_step":10,"fade_secs":0,"owner":"invented"}),
        json!({"kind":"clear_quiet_hours","max_volume":20}),
        json!({"kind":"unknown"}),
    ] {
        assert!(serde_json::from_value::<PolicyConfigurationChange>(value).is_err());
    }
    assert!(reply.calls.lock().unwrap().is_empty());
}

#[test]
fn configuration_tools_advertise_the_exact_tagged_change_and_write_semantics() {
    let server = fastmcp_server::ServerBuilder::try_new_with_fixed_protocol_policy(
        "policy-config-test",
        "1.0",
        fastmcp::ProtocolPolicy::Auto,
    )
    .unwrap()
    .tool(PolicyConfiguration)
    .tool(ConfigurePolicy)
    .try_build()
    .unwrap();
    let tools = server.tools();
    let read = serde_json::to_value(
        tools
            .iter()
            .find(|t| t.name == "policy_configuration")
            .unwrap(),
    )
    .unwrap();
    assert_eq!(read["annotations"]["readOnlyHint"], true);
    assert_eq!(read["annotations"]["idempotentHint"], true);
    let write =
        serde_json::to_value(tools.iter().find(|t| t.name == "configure_policy").unwrap()).unwrap();
    let required = write["inputSchema"]["required"].as_array().unwrap();
    assert_eq!(required.len(), 2);
    for key in ["expected_revision", "change"] {
        assert!(required.contains(&json!(key)));
    }
    let variants = write["inputSchema"]["properties"]["change"]["oneOf"]
        .as_array()
        .unwrap();
    assert_eq!(variants.len(), 7);
    for variant in variants {
        assert!(
            variant["required"]
                .as_array()
                .unwrap()
                .contains(&json!("kind"))
        );
        assert!(variant["properties"].get("type").is_none());
        assert_eq!(variant["additionalProperties"], false);
    }
    assert_ne!(write["annotations"]["readOnlyHint"], true);
    assert_ne!(write["annotations"]["idempotentHint"], true);
}
