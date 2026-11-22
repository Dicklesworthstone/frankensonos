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
        panic!("a forwarded, malformed or denied review must not contact local speakers")
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

fn fixture(answer: Option<Result<Value, Failure>>) -> (Backend, Arc<Reply>) {
    let reply = Arc::new(Reply {
        calls: Mutex::default(),
        answer,
    });
    (
        standalone(Policy::default(), Client::Unknown).with_daemon(reply.clone()),
        reply,
    )
}

fn standalone(policy: Policy, client: Client) -> Backend {
    Backend::new(
        Box::new(NoSpeakerIo),
        Box::new(|_| panic!("this request must fail before any standalone survey")),
        policy,
        client,
        Box::new(SystemClock),
    )
}

fn request(mode: ReviewedSourceMode, source: ReviewedSource) -> ReviewedSourceRequest {
    ReviewedSourceRequest {
        zone: "selected@S2".into(),
        expected_revision: "a".repeat(64),
        mode,
        source,
    }
}

fn sources() -> [ReviewedSource; 4] {
    [
        ReviewedSource::Uri {
            source_uri: "spotify:track:1000000000000000000001".into(),
            title: Some("Dvořák & friends".into()),
        },
        ReviewedSource::Favorite {
            favorite: "Evening favorites".into(),
        },
        ReviewedSource::Playlist {
            playlist: "Evening playlist".into(),
        },
        ReviewedSource::Tracks {
            tracks: [
                ("spotify:track:1000000000000000000001", "First movement"),
                (
                    "https://open.spotify.com/episode/1000000000000000000002?si=fixture",
                    "Selected episode",
                ),
                ("spotify:track:1000000000000000000001", "Repeated movement"),
            ]
            .map(|(source_uri, title)| QueueTrackInput {
                source_uri: source_uri.into(),
                title: Some(title.into()),
            })
            .to_vec(),
        },
    ]
}

fn modes() -> [ReviewedSourceMode; 3] {
    [
        ReviewedSourceMode::Play,
        ReviewedSourceMode::Next,
        ReviewedSourceMode::Append,
    ]
}

fn text(result: &FinalCallToolResult) -> String {
    serde_json::to_value(result).unwrap()["content"][0]["text"]
        .as_str()
        .unwrap()
        .into()
}

fn queued(complete: bool) -> Value {
    json!({
        "done": "added to selected queues", "complete": complete,
        "added": [{ "household": "S1", "coordinator": "RINCON_TEST_A", "queue": {
            "done": "queued in Study", "zone": "Study", "first": 4,
            "count": 1, "playing_queue": true
        }}],
        "failed": [], "logged": true, "undoable": true
    })
}

fn played() -> Value {
    json!({ "done": "started the selected group", "changed": true,
        "notes": [{ "code": "VOLUME_CLAMPED", "detail": "Study lowered first" }] })
}

#[test]
fn resolution_forwards_the_exact_encoded_target_and_exposes_legacy_review_identities() {
    let answer = json!({
        "target": "Ada’s & Studio@S2", "revision": "b".repeat(64), "matched": "alias",
        "rooms": [{ "room": "Studio", "household": "S2", "player": "RINCON_TEST_A",
            "coordinator": "RINCON_TEST_B" }],
        "groups": [{ "household": "S2", "coordinator": "RINCON_TEST_B",
            "coordinator_room": "Den", "members": [
                { "room": "Studio", "household": "S2", "player": "RINCON_TEST_A",
                    "coordinator": "RINCON_TEST_B" },
                { "room": "Den", "household": "S2", "player": "RINCON_TEST_B",
                    "coordinator": "RINCON_TEST_B" }
            ] }]
    });
    let (backend, reply) = fixture(Some(Ok(answer.clone())));
    let result = backend.resolve_rooms(" Ada’s & Studio@S2 ").unwrap();
    assert!(!result.is_error);
    assert_eq!(result.structured_content, Some(answer));
    let described = text(&result);
    for expected in [
        "Revision: ",
        &"b".repeat(64),
        "RINCON_TEST_A",
        "RINCON_TEST_B",
        "Den",
        "control_source",
    ] {
        assert!(described.contains(expected), "{described}");
    }
    assert_eq!(
        *reply.calls.lock().unwrap(),
        [json!([
            "GET",
            "/rooms/resolve?target=Ada%E2%80%99s%20%26%20Studio%40S2",
            null
        ])]
    );
}

#[test]
fn every_source_and_mode_forwards_its_revision_once_and_keeps_the_typed_receipt() {
    for mode in modes() {
        for source in sources() {
            let request = request(mode, source);
            let answer = if mode == ReviewedSourceMode::Play {
                played()
            } else {
                queued(true)
            };
            let (backend, reply) = fixture(Some(Ok(answer.clone())));
            let result = backend.control_source(&request).unwrap();
            assert!(!result.is_error);
            assert_eq!(result.structured_content, Some(answer));
            assert_eq!(
                *reply.calls.lock().unwrap(),
                [json!(["POST", "/sources/control", request])]
            );
            if mode == ReviewedSourceMode::Play {
                assert!(text(&result).contains("Study lowered first"));
            }
        }
    }
}

#[test]
fn partial_playback_and_queue_results_keep_every_target_and_signal_tool_failure() {
    let failure = Failure::new(ErrorCode::OutcomeUncertain, "destination reply lost").body();
    let playback = json!({
        "done": "Study started", "changed": true,
        "batch": { "complete": false, "results": [
            { "household": "S1", "target": "RINCON_TEST_A", "zone": "Study",
                "result": { "status": "succeeded", "done": "Study started", "changed": true } },
            { "household": "S2", "target": "RINCON_TEST_B", "zone": "Den",
                "result": { "status": "failed", "error": failure } }
        ] }
    });
    let mut queue = queued(false);
    queue["failed"] = json!([{ "household": "S2", "coordinator": "RINCON_TEST_B",
        "zone": "Den", "error": failure }]);
    queue["undoable"] = json!(false);
    for (mode, answer) in [
        (ReviewedSourceMode::Play, playback),
        (ReviewedSourceMode::Append, queue.clone()),
    ] {
        let request = request(mode, sources()[0].clone());
        let (backend, reply) = fixture(Some(Ok(answer.clone())));
        let result = backend.control_source(&request).unwrap();
        assert!(result.is_error);
        assert_eq!(result.structured_content, Some(answer));
        assert_eq!(reply.calls.lock().unwrap().len(), 1);
    }
    // A contradictory summary must not hide a concrete failed destination.
    queue["complete"] = json!(true);
    let (backend, _) = fixture(Some(Ok(queue)));
    assert!(
        backend
            .control_source(&request(ReviewedSourceMode::Next, sources()[0].clone()))
            .unwrap()
            .is_error
    );
}

#[test]
fn rejected_lost_and_wrong_mode_daemon_answers_never_fall_back_to_speaker_io() {
    for mode in modes() {
        let request = request(mode, sources()[0].clone());
        let wrong_mode = if mode == ReviewedSourceMode::Play {
            queued(true)
        } else {
            played()
        };
        for answer in [
            Err(Failure::new(ErrorCode::NotCoordinator, "review is stale")),
            Err(Failure::new(ErrorCode::PolicyDenied, "caller cannot play")),
            Err(Failure::new(ErrorCode::OutcomeUncertain, "reply lost")),
            Ok(json!({ "done": "unproved response" })),
            Ok(wrong_mode),
        ] {
            let malformed = answer.is_ok();
            let (backend, reply) = fixture(Some(answer));
            let error = backend.control_source(&request).unwrap_err();
            if malformed {
                assert!(error.message.contains("OUTCOME_UNCERTAIN"), "{error:?}");
            }
            assert_eq!(
                *reply.calls.lock().unwrap(),
                [json!(["POST", "/sources/control", request])]
            );
        }
    }
    for answer in [
        Err(Failure::new(ErrorCode::PolicyDenied, "cannot read groups")),
        Ok(json!({ "target": "selected" })),
    ] {
        let (backend, reply) = fixture(Some(answer));
        assert!(backend.resolve_rooms("selected").is_err());
        assert_eq!(reply.calls.lock().unwrap().len(), 1);
    }
}

#[test]
fn malformed_input_fails_before_the_daemon_is_contacted() {
    for revision in ["".into(), "A".repeat(64), "a".repeat(63)] {
        let mut request = request(ReviewedSourceMode::Play, sources()[0].clone());
        request.expected_revision = revision;
        let (backend, reply) = fixture(Some(Ok(played())));
        assert!(
            backend
                .control_source(&request)
                .unwrap_err()
                .message
                .contains("INVALID_ARGUMENT")
        );
        assert!(reply.calls.lock().unwrap().is_empty());
    }
    let (backend, reply) = fixture(None);
    assert!(
        backend
            .resolve_rooms(" ")
            .unwrap_err()
            .message
            .contains("INVALID_ARGUMENT")
    );
    assert!(reply.calls.lock().unwrap().is_empty());
    for (uri, favorite, playlist, title) in [
        (None, None, None, None),
        (Some("spotify:track:1"), Some("favorite"), None, None),
        (None, Some("favorite"), Some("playlist"), None),
        (None, Some("favorite"), None, Some("title")),
        (None, None, Some("playlist"), Some("title")),
    ] {
        assert!(
            source(
                uri.map(str::to_owned),
                favorite.map(str::to_owned),
                playlist.map(str::to_owned),
                title.map(str::to_owned),
                None,
            )
            .is_err()
        );
    }
    for value in ["", "queue", "PLAY", "play "] {
        assert!(checked_mode(value).is_err());
    }
}

#[test]
fn reviewed_track_choices_preserve_order_and_reject_ambiguous_or_invalid_inputs() {
    let track = json!({
        "source_uri": "spotify:track:1000000000000000000001",
        "title": "Selected first"
    });
    let episode = json!({
        "source_uri": "https://open.spotify.com/episode/1000000000000000000002?si=fixture",
        "title": "Selected episode"
    });
    let selected = json!([track, episode, track]);
    let tracks = serde_json::from_value(selected.clone()).unwrap();
    let chosen = source(None, None, None, None, Some(tracks)).unwrap();
    assert_eq!(
        serde_json::to_value(chosen).unwrap(),
        json!({"kind": "tracks", "tracks": selected})
    );
    for (uri, favorite, playlist, title) in [
        (
            Some("spotify:track:1000000000000000000001"),
            None,
            None,
            None,
        ),
        (None, Some("Evening"), None, None),
        (None, None, Some("Evening"), None),
        (None, None, None, Some("Shared title")),
    ] {
        assert!(
            source(
                uri.map(str::to_owned),
                favorite.map(str::to_owned),
                playlist.map(str::to_owned),
                title.map(str::to_owned),
                Some(serde_json::from_value(selected.clone()).unwrap()),
            )
            .is_err()
        );
    }
    for tracks in [
        json!([]),
        json!([{"source_uri": "spotify:album:1000000000000000000001"}]),
        json!([{"source_uri": "spotify:track:short"}]),
        json!(vec![track; 11]),
    ] {
        let request = request(
            ReviewedSourceMode::Append,
            ReviewedSource::Tracks {
                tracks: serde_json::from_value(tracks).unwrap(),
            },
        );
        let (backend, reply) = fixture(Some(Ok(queued(true))));
        assert!(backend.control_source(&request).is_err());
        assert!(reply.calls.lock().unwrap().is_empty());
    }
}

#[test]
fn standalone_reviews_enforce_read_permissions_and_each_actual_write_permission() {
    for denied in ["resolve_rooms", "list_rooms", "list_zones"] {
        let policy =
            Policy::from_toml(&format!("[clients.'mcp-stdio']\ndeny = ['{denied}']")).unwrap();
        let backend = standalone(policy, Client::McpStdio);
        assert!(
            backend
                .resolve_rooms("selected")
                .unwrap_err()
                .message
                .contains("POLICY_DENIED")
        );
    }
    for mode in modes() {
        for source in sources() {
            let permission = match (mode, &source) {
                (
                    ReviewedSourceMode::Play,
                    ReviewedSource::Uri { .. } | ReviewedSource::Tracks { .. },
                ) => "play",
                (ReviewedSourceMode::Play, _) => "play_favorite",
                _ => "add_to_queue",
            };
            let policy =
                Policy::from_toml(&format!("[clients.'mcp-stdio']\ndeny = ['{permission}']"))
                    .unwrap();
            let backend = standalone(policy, Client::McpStdio);
            let error = backend.control_source(&request(mode, source)).unwrap_err();
            assert!(error.message.contains("POLICY_DENIED"), "{error:?}");
        }
    }
}

#[test]
fn the_registered_review_workflow_requires_the_target_revision_and_marks_only_resolution_read_only()
{
    let definitions: Vec<_> = crate::server_with(Vec::new(), crate::Profile::Full)
        .tools()
        .into_iter()
        .map(|tool| serde_json::to_value(tool).unwrap())
        .collect();
    let resolve = definitions
        .iter()
        .find(|tool| tool["name"] == "resolve_rooms")
        .unwrap();
    assert_eq!(resolve["annotations"]["readOnlyHint"], true);
    assert_eq!(resolve["inputSchema"]["required"], json!(["target"]));
    let control = definitions
        .iter()
        .find(|tool| tool["name"] == "control_source")
        .unwrap();
    assert_ne!(control["annotations"]["readOnlyHint"], true);
    let required = control["inputSchema"]["required"].as_array().unwrap();
    assert_eq!(required.len(), 3);
    for field in ["zone", "expected_revision", "mode"] {
        assert!(required.contains(&json!(field)), "{control}");
    }
    for field in ["source_uri", "favorite", "playlist", "title", "tracks"] {
        assert!(control["inputSchema"]["properties"].get(field).is_some());
        assert!(!required.contains(&json!(field)));
    }
}
