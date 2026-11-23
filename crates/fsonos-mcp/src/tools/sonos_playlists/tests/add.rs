use super::*;
use fsonos_api::surface::queue::QueueTrackInput;
use fsonos_api::surface::sonos_playlists::AddTracksToSonosPlaylistRequest;

const TRACK: &str = "spotify:track:0000000000000000000001";
const EPISODE: &str = "spotify:episode:0000000000000000000002";

fn request() -> AddTracksToSonosPlaylistRequest {
    AddTracksToSonosPlaylistRequest {
        zone: "Den@S2".into(),
        playlist: "SQ:7".into(),
        expected_revision: "saved:destination&a=2".into(),
        tracks: [TRACK, EPISODE, TRACK]
            .into_iter()
            .map(|source_uri| QueueTrackInput {
                source_uri: source_uri.into(),
                title: Some("Selected item".into()),
            })
            .collect(),
    }
}

#[test]
fn explicit_saved_addition_forwards_once_and_retains_the_complete_receipt() {
    for logged in [false, true] {
        let receipt = json!({
            "done":"added selected sources", "zone":"Den",
            "playlist":{"id":"SQ:7","title":"Evening"},
            "revision":"saved:after", "previous_total":5, "total":8,
            "added":3, "source_uris":[TRACK, EPISODE, TRACK],
            "titles":["Selected item","Selected item","Selected item"], "logged":logged,
        });
        let reply = Arc::new(Reply {
            calls: Mutex::default(),
            answer: Some(Ok(receipt.clone())),
        });
        let request = request();
        let result = backend(&reply)
            .add_tracks_to_sonos_playlist(&request)
            .unwrap();
        assert!(!result.is_error);
        assert_eq!(result.structured_content, Some(receipt));
        let text = serde_json::to_string(&result.content).unwrap();
        assert!(text.contains("saved:after"));
        assert_eq!(text.contains("action log could not be written"), !logged);
        assert_eq!(
            *reply.calls.lock().unwrap(),
            [json!(["POST", "/sonos/playlists/add-tracks", request])]
        );
    }
}

#[test]
fn failed_or_malformed_saved_additions_never_replay_locally() {
    for answer in [
        Err(Failure::new(ErrorCode::QueueChanged, "destination changed")),
        Err(Failure::new(
            ErrorCode::OutcomeUncertain,
            "one of three additions confirmed; next reply lost",
        )),
        Err(Failure::new(ErrorCode::PlayerUnreachable, "not reachable")),
        Err(Failure::new(ErrorCode::PolicyDenied, "not permitted")),
        Ok(json!({"done":"missing verified append receipt"})),
    ] {
        let reply = Arc::new(Reply {
            calls: Mutex::default(),
            answer: Some(answer),
        });
        assert!(
            backend(&reply)
                .add_tracks_to_sonos_playlist(&request())
                .is_err()
        );
        assert_eq!(reply.calls.lock().unwrap().len(), 1);
    }
}

#[test]
fn invalid_saved_additions_stop_before_forwarding_and_direct_access_is_authorized() {
    let reply = Arc::new(Reply {
        calls: Mutex::default(),
        answer: None,
    });
    for request in [
        AddTracksToSonosPlaylistRequest {
            playlist: "Evening".into(),
            ..request()
        },
        AddTracksToSonosPlaylistRequest {
            expected_revision: " ".into(),
            ..request()
        },
        AddTracksToSonosPlaylistRequest {
            tracks: vec![],
            ..request()
        },
        AddTracksToSonosPlaylistRequest {
            tracks: vec![request().tracks[0].clone(); 11],
            ..request()
        },
        AddTracksToSonosPlaylistRequest {
            tracks: vec![QueueTrackInput {
                source_uri: "spotify:album:0000000000000000000001".into(),
                title: None,
            }],
            ..request()
        },
    ] {
        assert!(
            backend(&reply)
                .add_tracks_to_sonos_playlist(&request)
                .is_err()
        );
    }
    assert!(reply.calls.lock().unwrap().is_empty());
    assert!(
        backend(&reply)
            .add_tracks_to_sonos_playlist(&request())
            .unwrap_err()
            .message
            .contains("POLICY_DENIED")
    );
    assert_eq!(reply.calls.lock().unwrap().len(), 1);
}

#[test]
fn saved_addition_tool_requires_exact_destination_and_typed_sources() {
    let server = fastmcp_server::ServerBuilder::try_new_with_fixed_protocol_policy(
        "saved-playlist-sources-test",
        "1.0",
        fastmcp::ProtocolPolicy::Auto,
    )
    .unwrap()
    .tool(AddTracksToSonosPlaylist)
    .try_build()
    .unwrap();
    let tools = server.tools();
    let tool = tools
        .iter()
        .find(|tool| tool.name == "add_tracks_to_sonos_playlist")
        .unwrap();
    let value = serde_json::to_value(tool).unwrap();
    let required = value["inputSchema"]["required"].as_array().unwrap();
    for field in ["zone", "playlist", "expected_revision", "tracks"] {
        assert!(required.contains(&json!(field)), "{value}");
    }
    assert_eq!(
        value["inputSchema"]["properties"]["tracks"]["type"],
        "array"
    );
    assert_ne!(value["annotations"]["readOnlyHint"], true);
    assert_ne!(value["annotations"]["idempotentHint"], true);
}
