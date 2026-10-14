//! The `add_to_queue` and `play_queue` tools (see `fsonos_api::surface::queue`).

use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult};
use fsonos_api::surface::queue::{QueueAddRequest, QueuePlayRequest};
use fsonos_api::{ErrorCode, Failure, OutcomeDto};

use super::{Backend, respond, said, sentence, with_backend};

impl Backend {
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
}
