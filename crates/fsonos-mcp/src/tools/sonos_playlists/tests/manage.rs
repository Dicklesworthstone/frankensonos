use super::*;
use fsonos_api::surface::sonos_playlists::{
    RemoveSonosPlaylistRequest, RenameSonosPlaylistRequest,
};

fn rename_request() -> RenameSonosPlaylistRequest {
    RenameSonosPlaylistRequest {
        zone: "Den@S2".into(),
        playlist: "SQ:7".into(),
        expected_revision: "native:a&b=2".into(),
        title: "Café, A & B".into(),
    }
}

fn remove_request() -> RemoveSonosPlaylistRequest {
    RemoveSonosPlaylistRequest {
        zone: "Den@S2".into(),
        playlist: "SQ:7".into(),
        expected_revision: "native:a&b=2".into(),
    }
}

#[test]
fn native_catalog_changes_forward_once_with_exact_identity_revision_and_receipt() {
    for operation in ["rename", "remove"] {
        for logged in [false, true] {
            let outcome = json!({
                "done":"native catalog change confirmed", "zone":"Den",
                "playlist":{"id":"SQ:7", "title":"Café, A & B"},
                "operation":operation, "changed":true, "logged":logged,
            });
            let reply = Arc::new(Reply {
                calls: Mutex::default(),
                answer: Some(Ok(outcome.clone())),
            });
            let (result, body) = if operation == "rename" {
                (
                    backend(&reply).rename_sonos_playlist(&rename_request()),
                    json!(rename_request()),
                )
            } else {
                (
                    backend(&reply).remove_sonos_playlist(&remove_request()),
                    json!(remove_request()),
                )
            };
            let result = result.unwrap();
            assert!(!result.is_error);
            assert_eq!(result.structured_content, Some(outcome));
            let text = serde_json::to_string(&result.content).unwrap();
            assert_eq!(
                text.contains("action log could not be written"),
                !logged,
                "{text}"
            );
            assert_eq!(
                *reply.calls.lock().unwrap(),
                [json!([
                    "POST",
                    format!("/sonos/playlists/{operation}"),
                    body
                ]),]
            );
        }
    }
}

#[test]
fn native_catalog_failures_and_malformed_receipts_never_replay_through_local_speakers() {
    for answer in [
        Err(Failure::new(ErrorCode::QueueChanged, "playlist changed")),
        Err(Failure::new(
            ErrorCode::OutcomeUncertain,
            "write reply lost",
        )),
        Err(Failure::new(ErrorCode::PlayerUnreachable, "unreachable")),
        Err(Failure::new(ErrorCode::PolicyDenied, "not allowed")),
        Err(Failure::new(
            ErrorCode::UpnpFault,
            "native catalog write refused",
        )),
        Ok(json!({"done":"missing the native receipt"})),
        Ok(
            json!({"done":"malformed", "zone":"Den", "playlist":{"id":"SQ:7","title":"A"}, "operation":"overwrite", "changed":true, "logged":true}),
        ),
    ] {
        for operation in ["rename", "remove"] {
            let reply = Arc::new(Reply {
                calls: Mutex::default(),
                answer: Some(answer.clone()),
            });
            let result = if operation == "rename" {
                backend(&reply).rename_sonos_playlist(&rename_request())
            } else {
                backend(&reply).remove_sonos_playlist(&remove_request())
            };
            assert!(result.is_err());
            assert_eq!(reply.calls.lock().unwrap().len(), 1);
        }
    }
}

#[test]
fn invalid_catalog_mutations_do_not_forward_and_local_calls_require_permission() {
    let reply = Arc::new(Reply {
        calls: Mutex::default(),
        answer: None,
    });
    for (playlist, revision) in [("Evening", "r"), ("7", "r"), ("SQ:7/1", "r"), ("SQ:7", " ")] {
        let rename = RenameSonosPlaylistRequest {
            playlist: playlist.into(),
            expected_revision: revision.into(),
            ..rename_request()
        };
        let remove = RemoveSonosPlaylistRequest {
            playlist: playlist.into(),
            expected_revision: revision.into(),
            ..remove_request()
        };
        assert!(backend(&reply).rename_sonos_playlist(&rename).is_err());
        assert!(backend(&reply).remove_sonos_playlist(&remove).is_err());
    }
    for title in ["".into(), "A\nB".into(), "é".repeat(512)] {
        assert!(
            backend(&reply)
                .rename_sonos_playlist(&RenameSonosPlaylistRequest {
                    title,
                    ..rename_request()
                })
                .is_err()
        );
    }
    assert!(reply.calls.lock().unwrap().is_empty());
    assert!(
        backend(&reply)
            .rename_sonos_playlist(&rename_request())
            .unwrap_err()
            .message
            .contains("POLICY_DENIED")
    );
    assert!(
        backend(&reply)
            .remove_sonos_playlist(&remove_request())
            .unwrap_err()
            .message
            .contains("POLICY_DENIED")
    );
    assert_eq!(reply.calls.lock().unwrap().len(), 2);
}

#[test]
fn native_catalog_tools_register_required_proofs_as_non_idempotent_writes() {
    let server = fastmcp_server::ServerBuilder::try_new_with_fixed_protocol_policy(
        "native-catalog-test",
        "1.0",
        fastmcp::ProtocolPolicy::Auto,
    )
    .unwrap()
    .tool(RenameSonosPlaylist)
    .tool(RemoveSonosPlaylist)
    .try_build()
    .unwrap();
    for name in ["rename_sonos_playlist", "remove_sonos_playlist"] {
        let tools = server.tools();
        let tool = tools.iter().find(|tool| tool.name == name).unwrap();
        let value = serde_json::to_value(tool).unwrap();
        let required = value["inputSchema"]["required"].as_array().unwrap();
        for field in ["zone", "playlist", "expected_revision"] {
            assert!(required.contains(&json!(field)), "{value}");
        }
        assert_eq!(
            required.contains(&json!("title")),
            name == "rename_sonos_playlist"
        );
        assert_ne!(value["annotations"]["readOnlyHint"], true);
        assert_ne!(value["annotations"]["idempotentHint"], true);
    }
}
