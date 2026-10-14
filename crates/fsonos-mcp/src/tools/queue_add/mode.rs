//! Keep mode reads and changes on the daemon when it is present, so they
//! observe the running DJ and share the same playback snapshots and log.

use fastmcp::FinalCallToolResult;
use fastmcp::prelude::*;
use fsonos_api::surface::queue::{PlayModeDto, PlayModeRequest, PlayModeSetDto};
use fsonos_api::{ErrorCode, Failure};
use std::fmt::Write as _;

use super::super::{Backend, respond};

impl Backend {
    pub fn get_play_mode(&self, zone: &str) -> McpResult<FinalCallToolResult> {
        let path = mode_path(zone);
        if let Some(result) = self.on_daemon::<PlayModeDto>("GET", &path, None, PlayModeDto::text) {
            return result;
        }
        respond(|| {
            let state = self.surface.get_play_mode(&self.client, zone)?;
            Ok((state.text(), state))
        })
    }

    pub fn set_play_mode(&self, req: &PlayModeRequest) -> McpResult<FinalCallToolResult> {
        let body = serde_json::to_value(req).map_err(|error| {
            crate::tool_error(&Failure::new(ErrorCode::Internal, error.to_string()))
        })?;
        if let Some(result) = self.on_daemon::<PlayModeSetDto>(
            "POST",
            "/play-mode",
            Some(&body),
            PlayModeSetDto::text,
        ) {
            return result;
        }
        // A failed daemon reply never reaches here. It may describe an
        // already-applied change; do not retry it locally or drop expected_mode.
        respond(|| {
            let changed = self.surface.set_play_mode(&self.client, req)?;
            Ok((changed.text(), changed))
        })
    }
}

fn mode_path(zone: &str) -> String {
    let mut path = String::from("/zones/");
    for byte in zone.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            path.push(char::from(byte));
        } else {
            let _ = write!(path, "%{byte:02X}");
        }
    }
    path.push_str("/play-mode");
    path
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
            panic!("a forwarded or unauthorized mode change must not reach local speakers")
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
            Box::new(|_| panic!("no local survey is expected")),
            Policy::default(),
            Client::Unknown,
            Box::new(SystemClock),
        )
        .with_daemon(reply.clone())
    }

    fn req() -> PlayModeRequest {
        PlayModeRequest {
            zone: "Room@S2".into(),
            mode: "SHUFFLE_NOREPEAT".into(),
            expected_mode: Some("NORMAL".into()),
        }
    }

    #[test]
    fn reads_escape_one_room_segment_and_preserve_dj_ownership() {
        let state = json!({
            "zone": "A/B & É", "mode": "NORMAL", "queue_active": true, "dj_active": true
        });
        let reply = Arc::new(Reply {
            calls: Mutex::default(),
            answer: Some(Ok(state.clone())),
        });
        let result = backend(&reply).get_play_mode("A/B & É").unwrap();
        assert_eq!(result.structured_content, Some(state));
        assert_eq!(
            *reply.calls.lock().unwrap(),
            [json!([
                "GET",
                "/zones/A%2FB%20%26%20%C3%89/play-mode",
                null
            ])]
        );
    }

    #[test]
    fn mode_changes_forward_once_with_expected_state_and_confirmation() {
        let result = json!({
            "done": "queue uses SHUFFLE_NOREPEAT", "changed": true, "recovered": true,
            "state": { "zone": "Room", "mode": "SHUFFLE_NOREPEAT",
                       "queue_active": true, "dj_active": false }
        });
        let reply = Arc::new(Reply {
            calls: Mutex::default(),
            answer: Some(Ok(result.clone())),
        });
        let answer = backend(&reply).set_play_mode(&req()).unwrap();
        assert!(!answer.is_error);
        assert_eq!(answer.structured_content, Some(result));
        assert_eq!(
            *reply.calls.lock().unwrap(),
            [json!(["POST", "/play-mode", req()])]
        );
    }

    #[test]
    fn stale_refused_or_unreadable_daemon_results_never_trigger_a_local_write() {
        for result in [
            Err(Failure::invalid("the mode changed")),
            Err(Failure::new(ErrorCode::PlayerUnreachable, "lost response")),
            Err(Failure::new(ErrorCode::UpnpFault, "unconfirmed mode")),
            Err(Failure::new(ErrorCode::PolicyDenied, "not permitted")),
            Ok(json!({ "done": "missing confirmation" })),
        ] {
            let reply = Arc::new(Reply {
                calls: Mutex::default(),
                answer: Some(result),
            });
            assert!(backend(&reply).set_play_mode(&req()).is_err());
            assert_eq!(
                *reply.calls.lock().unwrap(),
                [json!(["POST", "/play-mode", req()])]
            );
        }
    }

    #[test]
    fn absent_daemon_still_enforces_local_identity_before_io() {
        let reply = Arc::new(Reply {
            calls: Mutex::default(),
            answer: None,
        });
        let error = backend(&reply).set_play_mode(&req()).unwrap_err();
        assert!(error.message.contains("POLICY_DENIED"));
        assert_eq!(reply.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn failed_mode_reads_do_not_silently_change_the_control_host() {
        for result in [
            Err(Failure::new(ErrorCode::NotReady, "still starting")),
            Ok(json!({ "mode": "NORMAL" })),
        ] {
            let reply = Arc::new(Reply {
                calls: Mutex::default(),
                answer: Some(result),
            });
            assert!(backend(&reply).get_play_mode("Room").is_err());
            assert_eq!(reply.calls.lock().unwrap().len(), 1);
        }
    }

    #[test]
    fn percent_and_fragment_characters_cannot_escape_the_room_path() {
        assert_eq!(mode_path("A%2FB?x#y"), "/zones/A%252FB%3Fx%23y/play-mode");
        assert_eq!(mode_path("a-b_c.d~9"), "/zones/a-b_c.d~9/play-mode");
    }
}
