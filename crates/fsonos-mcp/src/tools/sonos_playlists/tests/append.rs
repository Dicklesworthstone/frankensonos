use super::*;
use fsonos_api::surface::sonos_playlists::AppendSonosPlaylistTracksRequest;

fn request(source_playlist: Option<&str>) -> AppendSonosPlaylistTracksRequest {
    AppendSonosPlaylistTracksRequest {
        zone: "Den@S2".into(),
        playlist: "SQ:7".into(),
        expected_revision: "native:destination&a=2".into(),
        source_playlist: source_playlist.map(str::to_owned),
        source_revision: "source:observed&b=3".into(),
        positions: vec![2, 4, 2],
    }
}

#[test]
fn composition_forwards_both_proofs_and_ordered_occurrences_once() {
    for source in [None, Some("SQ:3")] {
        for logged in [false, true] {
            let receipt = json!({
                "done":"appended selected tracks", "zone":"Den",
                "playlist":{"id":"SQ:7","title":"Evening"},
                "revision":"native:after", "previous_total":5, "total":8,
                "added":3, "positions":[2,4,2],
                "titles":["Second","Fourth","Second"], "logged":logged,
            });
            let reply = Arc::new(Reply {
                calls: Mutex::default(),
                answer: Some(Ok(receipt.clone())),
            });
            let request = request(source);
            let result = backend(&reply)
                .append_sonos_playlist_tracks(&request)
                .unwrap();
            assert!(!result.is_error);
            assert_eq!(result.structured_content, Some(receipt));
            let text = serde_json::to_string(&result.content).unwrap();
            assert!(text.contains("native:after"));
            assert_eq!(text.contains("action log could not be written"), !logged);
            assert_eq!(
                *reply.calls.lock().unwrap(),
                [json!(["POST", "/sonos/playlists/append-tracks", request])]
            );
        }
    }
}

#[test]
fn failed_or_malformed_composition_never_replays_on_local_speakers() {
    for source in [None, Some("SQ:3")] {
        for answer in [
            Err(Failure::new(ErrorCode::QueueChanged, "source changed")),
            Err(Failure::new(ErrorCode::QueueChanged, "destination changed")),
            Err(Failure::new(
                ErrorCode::OutcomeUncertain,
                "append reply lost",
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
                    .append_sonos_playlist_tracks(&request(source))
                    .is_err()
            );
            assert_eq!(reply.calls.lock().unwrap().len(), 1);
        }
    }
}

#[test]
fn invalid_compositions_stop_before_forwarding_and_direct_writes_require_permission() {
    let reply = Arc::new(Reply {
        calls: Mutex::default(),
        answer: None,
    });
    for request in [
        AppendSonosPlaylistTracksRequest {
            playlist: "Evening".into(),
            ..request(None)
        },
        AppendSonosPlaylistTracksRequest {
            source_playlist: Some("Morning".into()),
            ..request(None)
        },
        AppendSonosPlaylistTracksRequest {
            expected_revision: " ".into(),
            ..request(None)
        },
        AppendSonosPlaylistTracksRequest {
            source_revision: " ".into(),
            ..request(None)
        },
        AppendSonosPlaylistTracksRequest {
            positions: vec![],
            ..request(None)
        },
        AppendSonosPlaylistTracksRequest {
            positions: vec![1; 11],
            ..request(None)
        },
        AppendSonosPlaylistTracksRequest {
            positions: vec![1, 0],
            ..request(None)
        },
        AppendSonosPlaylistTracksRequest {
            positions: vec![100_001],
            ..request(None)
        },
    ] {
        assert!(
            backend(&reply)
                .append_sonos_playlist_tracks(&request)
                .is_err()
        );
    }
    assert!(reply.calls.lock().unwrap().is_empty());
    for source in [None, Some("SQ:3")] {
        assert!(
            backend(&reply)
                .append_sonos_playlist_tracks(&request(source))
                .unwrap_err()
                .message
                .contains("POLICY_DENIED")
        );
    }
    assert_eq!(reply.calls.lock().unwrap().len(), 2);
}

#[test]
fn composition_requires_both_proofs_and_is_a_non_idempotent_mutation() {
    let server = fastmcp_server::ServerBuilder::try_new_with_fixed_protocol_policy(
        "native-playlist-composition-test",
        "1.0",
        fastmcp::ProtocolPolicy::Auto,
    )
    .unwrap()
    .tool(AppendSonosPlaylistTracks)
    .try_build()
    .unwrap();
    let tools = server.tools();
    let tool = tools
        .iter()
        .find(|tool| tool.name == "append_sonos_playlist_tracks")
        .unwrap();
    let value = serde_json::to_value(tool).unwrap();
    let required = value["inputSchema"]["required"].as_array().unwrap();
    for field in [
        "zone",
        "playlist",
        "expected_revision",
        "source_revision",
        "positions",
    ] {
        assert!(required.contains(&json!(field)), "{value}");
    }
    assert!(!required.contains(&json!("source_playlist")));
    assert_eq!(
        value["inputSchema"]["properties"]["positions"]["type"],
        "array"
    );
    assert_ne!(value["annotations"]["readOnlyHint"], true);
    assert_ne!(value["annotations"]["idempotentHint"], true);
}
