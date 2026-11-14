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
        panic!("room configuration never controls speakers")
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
        "revision":"aliases-v1-observed",
        "aliases":[{"name":"upstairs","rooms":["Study@SIM_S2","Offline fixture@SIM_S1"]}],
        "default_room":"Study@SIM_S2",
        "client_defaults":{"tag:fixture":"Offline fixture@SIM_S1"},
        "warnings":[{"line":3,"message":"An offline target was retained"}]
    })
}

fn changes() -> [RoomConfigurationChange; 4] {
    [
        RoomConfigurationChange::SetAlias {
            name: "upstairs".into(),
            rooms: vec!["Study@SIM_S2".into(), "Offline fixture@SIM_S1".into()],
        },
        RoomConfigurationChange::RemoveAlias {
            name: "upstairs".into(),
        },
        RoomConfigurationChange::SetDefault {
            room: "Study@SIM_S2".into(),
            client: Some("tag:fixture".into()),
        },
        RoomConfigurationChange::ClearDefault { client: None },
    ]
}

fn request(change: RoomConfigurationChange) -> RoomConfigureRequest {
    RoomConfigureRequest {
        expected_revision: "aliases-v1-observed".into(),
        change,
    }
}

#[test]
fn read_preserves_the_complete_remote_configuration_without_local_inventory() {
    let reply = Arc::new(Reply {
        calls: Mutex::default(),
        answer: Ok(configuration()),
    });
    let result = backend(&reply).room_configuration().unwrap();
    assert!(!result.is_error);
    assert_eq!(result.structured_content.unwrap(), configuration());
    assert_eq!(
        *reply.calls.lock().unwrap(),
        [json!(["GET", "/rooms/configuration", null])]
    );
}

#[test]
fn all_four_mutations_forward_one_exact_revision_and_change() {
    for change in changes() {
        let expected = request(change);
        let response = json!({"done":"Saved room configuration", "changed":true, "configuration":configuration()});
        let reply = Arc::new(Reply {
            calls: Mutex::default(),
            answer: Ok(response.clone()),
        });
        let result = backend(&reply).configure_rooms(&expected).unwrap();
        assert!(!result.is_error);
        assert_eq!(result.structured_content.unwrap(), response);
        assert_eq!(
            *reply.calls.lock().unwrap(),
            [json!(["POST", "/rooms/configuration", expected])]
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
                .configure_rooms(&request(changes()[0].clone()))
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
        request(RoomConfigurationChange::SetAlias {
            name: "here".into(),
            rooms: vec!["Study".into()],
        }),
        request(RoomConfigurationChange::SetAlias {
            name: "group".into(),
            rooms: vec![],
        }),
        request(RoomConfigurationChange::SetDefault {
            room: "all".into(),
            client: None,
        }),
    ] {
        assert!(backend(&reply).configure_rooms(&change).is_err());
        change.expected_revision.clear();
        assert!(backend(&reply).configure_rooms(&change).is_err());
    }
    let mut blank = request(changes()[0].clone());
    blank.expected_revision = " \n".into();
    assert!(backend(&reply).configure_rooms(&blank).is_err());
    for value in [
        json!({"kind":"set_alias","name":"group","rooms":["Study"],"force":true}),
        json!({"kind":"clear_default","room":"Study"}),
        json!({"kind":"unknown"}),
    ] {
        assert!(serde_json::from_value::<RoomConfigurationChange>(value).is_err());
    }
    assert!(reply.calls.lock().unwrap().is_empty());
}

#[test]
fn configuration_tools_advertise_the_exact_tagged_change_and_write_semantics() {
    let server = fastmcp_server::ServerBuilder::try_new_with_fixed_protocol_policy(
        "room-config-test",
        "1.0",
        fastmcp::ProtocolPolicy::Auto,
    )
    .unwrap()
    .tool(RoomConfiguration)
    .tool(ConfigureRooms)
    .try_build()
    .unwrap();
    let tools = server.tools();
    let read = serde_json::to_value(
        tools
            .iter()
            .find(|t| t.name == "room_configuration")
            .unwrap(),
    )
    .unwrap();
    assert_eq!(read["annotations"]["readOnlyHint"], true);
    assert_eq!(read["annotations"]["idempotentHint"], true);
    let write =
        serde_json::to_value(tools.iter().find(|t| t.name == "configure_rooms").unwrap()).unwrap();
    let required = write["inputSchema"]["required"].as_array().unwrap();
    assert_eq!(required.len(), 2);
    for key in ["expected_revision", "change"] {
        assert!(required.contains(&json!(key)));
    }
    let variants = write["inputSchema"]["properties"]["change"]["oneOf"]
        .as_array()
        .unwrap();
    assert_eq!(variants.len(), 4);
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
