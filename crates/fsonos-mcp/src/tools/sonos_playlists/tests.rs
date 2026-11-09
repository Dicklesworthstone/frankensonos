use super::*;
use crate::tools::Daemon;
use fsonos_api::{ErrorCode, Failure};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_proto::{ProtoError, Transport};
use serde_json::{Value, json};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

struct NoSpeakerIo;

impl Transport for NoSpeakerIo {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("forwarded or denied native playlist requests must not reach local speakers")
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
        Box::new(|_| panic!("playlist forwarding must not survey local speakers")),
        Policy::default(),
        Client::Unknown,
        Box::new(SystemClock),
    )
    .with_daemon(reply.clone())
}

fn request() -> PlaySonosPlaylistRequest {
    PlaySonosPlaylistRequest {
        zone: "Den@S2".into(),
        playlist: "SQ:7".into(),
    }
}

fn contents_request() -> SonosPlaylistContentsRequest {
    SonosPlaylistContentsRequest {
        zone: "Den & Study@S2".into(),
        playlist: "SQ:7".into(),
        offset: 2,
        limit: 25,
        expected_revision: Some("native:a&b=2".into()),
    }
}

fn track_request() -> SonosPlaylistTrackRequest {
    SonosPlaylistTrackRequest {
        zone: "Den@S2".into(),
        playlist: "SQ:7".into(),
        position: 3,
        expected_revision: "native:a&b=2".into(),
        mode: fsonos_api::surface::sonos_playlists::SonosPlaylistTrackMode::Next,
    }
}

#[test]
fn selected_track_forwards_the_exact_household_occurrence_and_revision_once() {
    let outcome = json!({
        "done":"queued the selected native track", "zone":"Den", "playlist":"SQ:7",
        "position":3, "title":"Repeat", "mode":"next", "changed":true, "notes":[],
        "first":2, "count":1, "playing_queue":true,
    });
    let reply = Arc::new(Reply {
        calls: Mutex::default(),
        answer: Some(Ok(outcome.clone())),
    });
    let result = backend(&reply)
        .sonos_playlist_track(&track_request())
        .unwrap();
    assert!(!result.is_error);
    assert_eq!(result.structured_content, Some(outcome));
    assert_eq!(
        *reply.calls.lock().unwrap(),
        [json!(["POST", "/sonos/playlists/track", track_request()])]
    );
}

#[test]
fn selected_track_failure_or_malformed_receipt_never_replays_locally() {
    for answer in [
        Err(Failure::new(ErrorCode::QueueChanged, "playlist changed")),
        Err(Failure::new(
            ErrorCode::OutcomeUncertain,
            "write reply lost",
        )),
        Err(Failure::new(
            ErrorCode::PlayerUnreachable,
            "speaker unavailable",
        )),
        Err(Failure::new(ErrorCode::PolicyDenied, "not allowed")),
        Ok(json!({"done":"missing selection receipt"})),
        Ok(
            json!({"done":"invalid mode", "zone":"Den", "playlist":"SQ:7", "position":3, "title":"Track", "mode":"replace", "changed":true}),
        ),
    ] {
        let reply = Arc::new(Reply {
            calls: Mutex::default(),
            answer: Some(answer),
        });
        assert!(
            backend(&reply)
                .sonos_playlist_track(&track_request())
                .is_err()
        );
        assert_eq!(
            *reply.calls.lock().unwrap(),
            [json!(["POST", "/sonos/playlists/track", track_request()])]
        );
    }
    let reply = Arc::new(Reply {
        calls: Mutex::default(),
        answer: None,
    });
    for request in [
        SonosPlaylistTrackRequest {
            position: 0,
            ..track_request()
        },
        SonosPlaylistTrackRequest {
            playlist: "Evening".into(),
            ..track_request()
        },
        SonosPlaylistTrackRequest {
            expected_revision: String::new(),
            ..track_request()
        },
    ] {
        assert!(backend(&reply).sonos_playlist_track(&request).is_err());
    }
    assert!(reply.calls.lock().unwrap().is_empty());
    assert!(
        backend(&reply)
            .sonos_playlist_track(&track_request())
            .unwrap_err()
            .message
            .contains("POLICY_DENIED")
    );
    assert_eq!(reply.calls.lock().unwrap().len(), 1);
}

#[test]
fn selected_track_tool_registers_with_its_mandatory_selection_fields() {
    let server = fastmcp_server::ServerBuilder::try_new_with_fixed_protocol_policy(
        "native-track-test",
        "1.0",
        fastmcp::ProtocolPolicy::Auto,
    )
    .unwrap()
    .tool(SonosPlaylistTrack)
    .try_build()
    .unwrap();
    let tools = server.tools();
    let tool = tools
        .iter()
        .find(|tool| tool.name == "sonos_playlist_track")
        .unwrap();
    let value = serde_json::to_value(tool).unwrap();
    let required = value["inputSchema"]["required"].as_array().unwrap();
    for field in ["zone", "playlist", "position", "expected_revision", "mode"] {
        assert!(required.contains(&json!(field)), "{value}");
    }
    assert_ne!(value["annotations"]["readOnlyHint"], true);
    assert_ne!(value["annotations"]["idempotentHint"], true);
}

#[test]
fn native_contents_forward_exact_daemon_household_page_and_revision() {
    let page = json!({
        "zone":"Den & Study", "playlist":{"id":"SQ:7", "title":"Evening & Friends"},
        "revision":"native:a&b=2", "offset":2, "total":3,
        "items":[{"position":3,"title":"Repeat","creator":"Artist", "duration_secs":123}]
    });
    let reply = Arc::new(Reply {
        calls: Mutex::default(),
        answer: Some(Ok(page.clone())),
    });
    let result = backend(&reply)
        .sonos_playlist_contents(&contents_request())
        .unwrap();
    assert!(!result.is_error);
    assert_eq!(result.structured_content, Some(page));
    assert_eq!(
        *reply.calls.lock().unwrap(),
        [json!([
            "GET",
            "/sonos/playlists/contents?zone=Den%20%26%20Study%40S2&playlist=SQ%3A7&offset=2&limit=25&expected_revision=native%3Aa%26b%3D2",
            null
        ])]
    );
}

#[test]
fn failed_or_malformed_contents_never_fall_back_to_another_household() {
    for answer in [
        Err(Failure::new(
            ErrorCode::QueueChanged,
            "saved playlist changed",
        )),
        Err(Failure::new(
            ErrorCode::PlayerUnreachable,
            "speaker unavailable",
        )),
        Err(Failure::new(ErrorCode::PolicyDenied, "not allowed")),
        Ok(json!({"zone":"Den", "playlist":{"id":"SQ:7", "title":"Incomplete"}, "items":[]})),
    ] {
        let reply = Arc::new(Reply {
            calls: Mutex::default(),
            answer: Some(answer),
        });
        assert!(
            backend(&reply)
                .sonos_playlist_contents(&contents_request())
                .is_err()
        );
        assert_eq!(reply.calls.lock().unwrap().len(), 1);
    }
    let reply = Arc::new(Reply {
        calls: Mutex::default(),
        answer: None,
    });
    let mut request = contents_request();
    request.expected_revision = None;
    assert!(backend(&reply).sonos_playlist_contents(&request).is_err());
    assert!(reply.calls.lock().unwrap().is_empty());
}

#[test]
fn native_playlist_reads_keep_the_selected_daemons_household_and_native_ids() {
    let list = json!([{"id":"SQ:7", "title":"Evening & Friends"}]);
    let reply = Arc::new(Reply {
        calls: Mutex::default(),
        answer: Some(Ok(list.clone())),
    });
    let result = backend(&reply).list_sonos_playlists("Den@S2").unwrap();
    assert!(!result.is_error);
    assert_eq!(result.structured_content, Some(json!({"playlists":list})));
    assert_eq!(
        *reply.calls.lock().unwrap(),
        [json!(["GET", "/sonos/playlists?zone=Den%40S2", null])]
    );
}

#[test]
fn native_playlist_play_forwards_only_the_household_and_selection_once() {
    let outcome = json!({"done":"playing the Sonos playlist", "changed":true});
    let reply = Arc::new(Reply {
        calls: Mutex::default(),
        answer: Some(Ok(outcome.clone())),
    });
    let result = backend(&reply).play_sonos_playlist(&request()).unwrap();
    assert!(!result.is_error);
    assert_eq!(result.structured_content, Some(outcome));
    assert_eq!(
        *reply.calls.lock().unwrap(),
        [json!(["POST", "/sonos/playlists/play", request()])]
    );
}

#[test]
fn failed_or_malformed_playlist_replies_never_replay_locally() {
    for answer in [
        Err(Failure::new(ErrorCode::PlayerUnreachable, "reply lost")),
        Err(Failure::new(
            ErrorCode::OutcomeUncertain,
            "unknown queue outcome",
        )),
        Err(Failure::new(ErrorCode::PolicyDenied, "not allowed")),
        Ok(json!({"done":"missing outcome"})),
    ] {
        let reply = Arc::new(Reply {
            calls: Mutex::default(),
            answer: Some(answer),
        });
        assert!(backend(&reply).play_sonos_playlist(&request()).is_err());
        assert_eq!(reply.calls.lock().unwrap().len(), 1);
    }
}

#[test]
fn invalid_playlists_are_not_sent_and_unknown_local_callers_are_denied() {
    let reply = Arc::new(Reply {
        calls: Mutex::default(),
        answer: None,
    });
    let mut req = request();
    req.playlist.clear();
    assert!(backend(&reply).play_sonos_playlist(&req).is_err());
    assert!(reply.calls.lock().unwrap().is_empty());
    assert!(
        backend(&reply)
            .play_sonos_playlist(&request())
            .unwrap_err()
            .message
            .contains("POLICY_DENIED")
    );
    assert_eq!(reply.calls.lock().unwrap().len(), 1);
}

fn save_request() -> SaveSonosPlaylistRequest {
    SaveSonosPlaylistRequest {
        zone: "Den@S2".into(),
        title: "Evening & Friends".into(),
    }
}

#[test]
fn saving_forwards_once_and_preserves_the_confirmed_native_identity_and_log_status() {
    for logged in [false, true] {
        let outcome = json!({
            "done":"saved the queue as a new Sonos playlist", "zone":"Den",
            "playlist":{"id":"SQ:8", "title":"Evening & Friends"}, "logged":logged
        });
        let reply = Arc::new(Reply {
            calls: Mutex::default(),
            answer: Some(Ok(outcome.clone())),
        });
        let result = backend(&reply)
            .save_sonos_playlist(&save_request())
            .unwrap();
        assert!(!result.is_error);
        assert_eq!(result.structured_content, Some(outcome));
        assert_eq!(
            *reply.calls.lock().unwrap(),
            [json!(["POST", "/sonos/playlists/save", save_request()])]
        );
    }
}

#[test]
fn uncertain_refused_or_malformed_saves_never_create_a_second_playlist() {
    for answer in [
        Err(Failure::new(ErrorCode::OutcomeUncertain, "save reply lost")),
        Err(Failure::new(ErrorCode::PlayerUnreachable, "unreachable")),
        Err(Failure::new(ErrorCode::PolicyDenied, "save not allowed")),
        Err(Failure::new(ErrorCode::UpnpFault, "refused save")),
        Ok(json!({"done":"missing the saved identity"})),
        Ok(json!({"done":"saved", "zone":"Den", "playlist":{"id":"SQ:8"}, "logged":true})),
    ] {
        let reply = Arc::new(Reply {
            calls: Mutex::default(),
            answer: Some(answer),
        });
        assert!(
            backend(&reply)
                .save_sonos_playlist(&save_request())
                .is_err()
        );
        assert_eq!(
            *reply.calls.lock().unwrap(),
            [json!(["POST", "/sonos/playlists/save", save_request()])]
        );
    }
}

#[test]
fn invalid_save_titles_are_not_forwarded_and_standalone_saves_still_require_permission() {
    let reply = Arc::new(Reply {
        calls: Mutex::default(),
        answer: None,
    });
    for title in ["".into(), "A\nB".into(), "é".repeat(512)] {
        let request = SaveSonosPlaylistRequest {
            title,
            ..save_request()
        };
        assert!(backend(&reply).save_sonos_playlist(&request).is_err());
    }
    assert!(reply.calls.lock().unwrap().is_empty());
    assert!(
        backend(&reply)
            .save_sonos_playlist(&save_request())
            .unwrap_err()
            .message
            .contains("POLICY_DENIED")
    );
    assert_eq!(reply.calls.lock().unwrap().len(), 1);
}
