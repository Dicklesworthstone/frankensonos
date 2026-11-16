use super::*;
use fsonos_api::surface::sonos_playlists::{
    MoveSonosPlaylistTracksRequest, RemoveSonosPlaylistTracksRequest,
};

fn remove() -> RemoveSonosPlaylistTracksRequest {
    RemoveSonosPlaylistTracksRequest {
        zone: "Den@S2".into(),
        playlist: "SQ:7".into(),
        expected_revision: "native:a&b=2".into(),
        position: 2,
        count: 3,
    }
}

fn movement() -> MoveSonosPlaylistTracksRequest {
    MoveSonosPlaylistTracksRequest {
        zone: "Den@S2".into(),
        playlist: "SQ:7".into(),
        expected_revision: "native:a&b=2".into(),
        position: 2,
        count: 3,
        insert_before: 8,
    }
}

#[test]
fn occurrence_edits_forward_exact_identity_range_and_original_destination_once() {
    for moving in [false, true] {
        for logged in [false, true] {
            let mut receipt = json!({
                "done":"saved playlist edited", "zone":"Den",
                "playlist":{"id":"SQ:7", "title":"Evening"},
                "operation":if moving {"move_tracks"} else {"remove_tracks"},
                "position":2, "count":3, "previous_total":8,
                "total":if moving {8} else {5}, "revision":"native:after",
                "changed":true, "logged":logged,
            });
            if moving {
                receipt["insert_before"] = json!(8);
            }
            let reply = Arc::new(Reply {
                calls: Mutex::default(),
                answer: Some(Ok(receipt.clone())),
            });
            let (result, path, body) = if moving {
                (
                    backend(&reply).move_sonos_playlist_tracks(&movement()),
                    "/sonos/playlists/move-tracks",
                    json!(movement()),
                )
            } else {
                (
                    backend(&reply).remove_sonos_playlist_tracks(&remove()),
                    "/sonos/playlists/remove-tracks",
                    json!(remove()),
                )
            };
            let result = result.unwrap();
            assert!(!result.is_error);
            assert_eq!(result.structured_content, Some(receipt));
            let text = serde_json::to_string(&result.content).unwrap();
            assert!(text.contains("native:after"));
            assert_eq!(text.contains("action log could not be written"), !logged);
            assert_eq!(*reply.calls.lock().unwrap(), [json!(["POST", path, body])]);
        }
    }
}

#[test]
fn uncertain_edit_or_malformed_receipt_never_replays_on_local_speakers() {
    for answer in [
        Err(Failure::new(
            ErrorCode::QueueChanged,
            "saved contents changed",
        )),
        Err(Failure::new(
            ErrorCode::OutcomeUncertain,
            "write reply lost",
        )),
        Err(Failure::new(ErrorCode::PlayerUnreachable, "not reachable")),
        Err(Failure::new(ErrorCode::PolicyDenied, "not permitted")),
        Ok(json!({"done":"missing verified contents receipt"})),
        Ok(json!({
            "done":"invalid operation", "zone":"Den", "playlist":{"id":"SQ:7","title":"Evening"},
            "operation":"replace_queue", "position":2, "count":3, "previous_total":8,
            "total":5, "revision":"after", "changed":true, "logged":true,
        })),
    ] {
        for moving in [false, true] {
            let reply = Arc::new(Reply {
                calls: Mutex::default(),
                answer: Some(answer.clone()),
            });
            let result = if moving {
                backend(&reply).move_sonos_playlist_tracks(&movement())
            } else {
                backend(&reply).remove_sonos_playlist_tracks(&remove())
            };
            assert!(result.is_err());
            assert_eq!(reply.calls.lock().unwrap().len(), 1);
        }
    }
}

#[test]
fn invalid_occurrence_requests_stop_before_forwarding_and_local_writes_require_permission() {
    let reply = Arc::new(Reply {
        calls: Mutex::default(),
        answer: None,
    });
    for request in [
        RemoveSonosPlaylistTracksRequest {
            playlist: "Evening".into(),
            ..remove()
        },
        RemoveSonosPlaylistTracksRequest {
            expected_revision: " ".into(),
            ..remove()
        },
        RemoveSonosPlaylistTracksRequest {
            position: 0,
            ..remove()
        },
        RemoveSonosPlaylistTracksRequest {
            count: 0,
            ..remove()
        },
        RemoveSonosPlaylistTracksRequest {
            count: 1001,
            ..remove()
        },
        RemoveSonosPlaylistTracksRequest {
            position: u32::MAX,
            count: 2,
            ..remove()
        },
    ] {
        assert!(
            backend(&reply)
                .remove_sonos_playlist_tracks(&request)
                .is_err()
        );
    }
    assert!(
        backend(&reply)
            .move_sonos_playlist_tracks(&MoveSonosPlaylistTracksRequest {
                insert_before: 0,
                ..movement()
            })
            .is_err()
    );
    assert!(reply.calls.lock().unwrap().is_empty());
    assert!(
        backend(&reply)
            .remove_sonos_playlist_tracks(&remove())
            .unwrap_err()
            .message
            .contains("POLICY_DENIED")
    );
    assert!(
        backend(&reply)
            .move_sonos_playlist_tracks(&movement())
            .unwrap_err()
            .message
            .contains("POLICY_DENIED")
    );
    assert_eq!(reply.calls.lock().unwrap().len(), 2);
}

#[test]
fn saved_occurrence_tools_require_proofs_and_are_not_idempotent() {
    let server = fastmcp_server::ServerBuilder::try_new_with_fixed_protocol_policy(
        "native-playlist-edits-test",
        "1.0",
        fastmcp::ProtocolPolicy::Auto,
    )
    .unwrap()
    .tool(RemoveSonosPlaylistTracks)
    .tool(MoveSonosPlaylistTracks)
    .try_build()
    .unwrap();
    for name in ["remove_sonos_playlist_tracks", "move_sonos_playlist_tracks"] {
        let tools = server.tools();
        let tool = tools.iter().find(|tool| tool.name == name).unwrap();
        let value = serde_json::to_value(tool).unwrap();
        let required = value["inputSchema"]["required"].as_array().unwrap();
        for field in ["zone", "playlist", "expected_revision", "position", "count"] {
            assert!(required.contains(&json!(field)), "{value}");
        }
        assert_eq!(
            required.contains(&json!("insert_before")),
            name == "move_sonos_playlist_tracks"
        );
        assert_ne!(value["annotations"]["readOnlyHint"], true);
        assert_ne!(value["annotations"]["idempotentHint"], true);
    }
}
