//! Queue addition, playback and removal (see `fsonos_api::surface::queue`).

use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult};
use fsonos_api::surface::queue::{
    QueueAddRequest, QueueMoveRequest, QueueMovedDto, QueuePlayRequest, QueueRemoveRequest,
    QueueRemovedDto,
};
use fsonos_api::{ErrorCode, Failure, OutcomeDto};

use super::{Backend, respond, said, sentence, with_backend};

mod mode;

impl Backend {
    /// Send a move through the daemon once. A failure cannot authorize a
    /// second local edit or dropping the required revision/identity checks.
    pub fn move_in_queue(&self, req: &QueueMoveRequest) -> McpResult<FinalCallToolResult> {
        let body = serde_json::to_value(req).map_err(|error| {
            crate::tool_error(&Failure::new(ErrorCode::Internal, error.to_string()))
        })?;
        if let Some(result) =
            self.on_daemon::<QueueMovedDto>("POST", "/queue/move", Some(&body), |moved| {
                moved.done.clone()
            })
        {
            return result;
        }
        respond(|| {
            let moved = self.surface.queue_move(&self.client, req)?;
            Ok((moved.done.clone(), moved))
        })
    }

    /// Keep queue deletion on the daemon when present. A failed or malformed
    /// daemon response is terminal: it may have already removed the range.
    pub fn remove_from_queue(&self, req: &QueueRemoveRequest) -> McpResult<FinalCallToolResult> {
        let body = serde_json::to_value(req).map_err(|error| {
            crate::tool_error(&Failure::new(ErrorCode::Internal, error.to_string()))
        })?;
        if let Some(result) =
            self.on_daemon::<QueueRemovedDto>("POST", "/queue/remove", Some(&body), |removed| {
                removed.done.clone()
            })
        {
            return result;
        }
        respond(|| {
            let removed = self.surface.queue_remove(&self.client, req)?;
            Ok((removed.done.clone(), removed))
        })
    }

    /// Prefer the running daemon so playback and undo share its action log.
    /// A daemon error is returned, never retried through a second control path.
    pub fn play_queue(&self, req: &QueuePlayRequest) -> McpResult<FinalCallToolResult> {
        let body = serde_json::to_value(req).map_err(|error| {
            crate::tool_error(&Failure::new(ErrorCode::Internal, error.to_string()))
        })?;
        if let Some(result) =
            self.on_daemon::<OutcomeDto>("POST", "/queue/play", Some(&body), sentence)
        {
            return result;
        }
        respond(|| self.surface.queue_play(&self.client, req).map(said))
    }

    /// The `add_to_queue` tool.
    pub fn add_to_queue(&self, req: &QueueAddRequest) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let added = self.surface.queue_add(&self.client, req)?;
            Ok((added.done.clone(), added))
        })
    }
}

#[tool(
    description = "Add a Spotify track, album or playlist (`spotify:track|album|playlist:<id>`, or an open.spotify.com link) to the queue of the group a room plays in: at the end, or with `next` true right after the track playing. An album or playlist adds all its tracks; `title` names it in the queue. Undoable with undo_last. `zone` is a room name, Room@S1 / Room@S2, or one of your aliases."
)]
fn add_to_queue(
    _ctx: &McpContext,
    zone: String,
    source_uri: String,
    title: Option<String>,
    next: Option<bool>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend(move |b| {
        b.add_to_queue(&QueueAddRequest {
            zone,
            source_uri,
            title,
            next: next.unwrap_or(false),
        })
    })
}

#[cfg(test)]
mod tests {
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
            panic!("a forwarded or denied queue selection must not contact local speakers")
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
            Box::new(|_| panic!("these tests never survey the LAN")),
            Policy::default(),
            Client::Unknown,
            Box::new(SystemClock),
        )
        .with_daemon(reply.clone())
    }

    fn request() -> QueuePlayRequest {
        QueuePlayRequest {
            zone: "Room@S2".into(),
            position: 3,
            expected_uri: Some("x-file-cifs://example.invalid/selected.flac".into()),
            expected_revision: Some("q1:9:test:room:7:3".into()),
        }
    }

    #[test]
    fn queue_selection_forwards_once_and_keeps_structured_outcome_and_cap_notes() {
        let outcome = json!({
            "done": "playing selected queue entry", "changed": true,
            "notes": [{ "code": "VOLUME_CLAMPED", "detail": "Room lowered first" }]
        });
        let reply = Arc::new(Reply {
            calls: Mutex::default(),
            answer: Some(Ok(outcome.clone())),
        });
        let result = backend(&reply).play_queue(&request()).unwrap();
        assert!(!result.is_error);
        assert_eq!(result.structured_content, Some(outcome));
        assert_eq!(
            *reply.calls.lock().unwrap(),
            [json!(["POST", "/queue/play", request()])]
        );
    }

    #[test]
    fn an_ambiguous_daemon_failure_never_falls_back_to_local_playback() {
        let reply = Arc::new(Reply {
            calls: Mutex::default(),
            answer: Some(Err(Failure::new(
                ErrorCode::PlayerUnreachable,
                "reply was lost",
            ))),
        });
        let error = backend(&reply).play_queue(&request()).unwrap_err();
        assert!(error.message.contains("PLAYER_UNREACHABLE"));
        assert_eq!(reply.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn without_a_daemon_the_shared_surface_still_enforces_local_identity() {
        let reply = Arc::new(Reply {
            calls: Mutex::default(),
            answer: None,
        });
        let error = backend(&reply).play_queue(&request()).unwrap_err();
        assert!(error.message.contains("POLICY_DENIED"));
        assert_eq!(reply.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn stale_revision_is_not_retried_or_downgraded_to_uri_only() {
        let reply = Arc::new(Reply {
            calls: Mutex::default(),
            answer: Some(Err(Failure::new(ErrorCode::NotReady, "the queue changed"))),
        });
        let error = backend(&reply).play_queue(&request()).unwrap_err();
        assert!(error.message.contains("NOT_READY"));
        let calls = reply.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0][2]["expected_revision"], "q1:9:test:room:7:3");
    }

    fn removal() -> QueueRemoveRequest {
        QueueRemoveRequest {
            zone: "Room@S2".into(),
            position: 3,
            expected_uris: vec![
                "x-sonos-spotify:spotify%3Atrack%3AExactIdentity?sid=12&sn=2".into(),
                "x-file-cifs://example.invalid/second.flac".into(),
            ],
            expected_revision: Some("opaque-token-from-get-queue".into()),
        }
    }

    #[test]
    fn removal_forwards_exact_identities_and_revision_and_returns_confirmed_counts() {
        let outcome = json!({
            "done": "removed 2 tracks (not undoable)", "zone": "Room", "position": 3,
            "count": 2, "remaining": 8, "recovered": true, "undoable": false
        });
        let reply = Arc::new(Reply {
            calls: Mutex::default(),
            answer: Some(Ok(outcome.clone())),
        });
        let result = backend(&reply).remove_from_queue(&removal()).unwrap();
        assert!(!result.is_error);
        assert_eq!(result.structured_content, Some(outcome));
        assert_eq!(
            *reply.calls.lock().unwrap(),
            [json!(["POST", "/queue/remove", removal()])]
        );
    }

    #[test]
    fn every_daemon_removal_failure_is_terminal_without_local_replay() {
        for code in [
            ErrorCode::PlayerUnreachable,
            ErrorCode::UpnpFault,
            ErrorCode::PolicyDenied,
            ErrorCode::InvalidArgument,
        ] {
            let reply = Arc::new(Reply {
                calls: Mutex::default(),
                answer: Some(Err(Failure::new(code, "synthetic daemon failure"))),
            });
            let error = backend(&reply).remove_from_queue(&removal()).unwrap_err();
            assert!(error.message.contains(code.as_str()));
            assert_eq!(reply.calls.lock().unwrap().len(), 1);
        }
    }

    #[test]
    fn malformed_daemon_removal_success_is_not_a_reason_to_remove_locally() {
        let reply = Arc::new(Reply {
            calls: Mutex::default(),
            answer: Some(Ok(json!({ "done": "missing confirmation fields" }))),
        });
        assert!(backend(&reply).remove_from_queue(&removal()).is_err());
        assert_eq!(reply.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn standalone_removal_still_checks_the_local_house_policy_before_speaker_io() {
        let reply = Arc::new(Reply {
            calls: Mutex::default(),
            answer: None,
        });
        let error = backend(&reply).remove_from_queue(&removal()).unwrap_err();
        assert!(error.message.contains("POLICY_DENIED"));
        assert_eq!(reply.calls.lock().unwrap().len(), 1);
    }

    fn movement() -> QueueMoveRequest {
        QueueMoveRequest {
            zone: "Room@S2".into(),
            position: 3,
            insert_before: 1,
            expected_uris: vec![
                "x-sonos-spotify:spotify%3Atrack%3AExactIdentity?sid=12&sn=2".into(),
                "x-file-cifs://example.invalid/second.flac".into(),
            ],
            expected_revision: "opaque-coordinator-scoped-token".into(),
        }
    }

    #[test]
    fn moves_forward_the_exact_selection_and_return_the_confirmed_new_revision() {
        let outcome = json!({
            "done": "moved two queue entries", "zone": "Room", "position": 1,
            "count": 2, "total": 8, "changed": true, "recovered": true,
            "undoable": false, "revision": "new-opaque-token"
        });
        let reply = Arc::new(Reply {
            calls: Mutex::default(),
            answer: Some(Ok(outcome.clone())),
        });
        let result = backend(&reply).move_in_queue(&movement()).unwrap();
        assert!(!result.is_error);
        assert_eq!(result.structured_content, Some(outcome));
        assert_eq!(
            *reply.calls.lock().unwrap(),
            [json!(["POST", "/queue/move", movement()])]
        );
    }

    #[test]
    fn failed_or_malformed_daemon_moves_never_replay_or_drop_preconditions() {
        let answers = [
            Err(Failure::new(ErrorCode::PlayerUnreachable, "reply lost")),
            Err(Failure::invalid("stale revision")),
            Err(Failure::new(ErrorCode::UpnpFault, "uncertain move")),
            Ok(json!({ "done": "missing confirmation fields" })),
        ];
        for answer in answers {
            let reply = Arc::new(Reply {
                calls: Mutex::default(),
                answer: Some(answer),
            });
            assert!(backend(&reply).move_in_queue(&movement()).is_err());
            assert_eq!(
                *reply.calls.lock().unwrap(),
                [json!(["POST", "/queue/move", movement()])]
            );
        }
    }

    #[test]
    fn standalone_queue_moves_check_identity_before_any_speaker_io() {
        let reply = Arc::new(Reply {
            calls: Mutex::default(),
            answer: None,
        });
        let error = backend(&reply).move_in_queue(&movement()).unwrap_err();
        assert!(error.message.contains("POLICY_DENIED"));
        assert_eq!(reply.calls.lock().unwrap().len(), 1);
    }
}
