//! The `events` tool: the house's changes after an event id, waiting a while
//! for one (a long poll on the daemon's event stream; see
//! `fsonos_api::events`).

use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult, ResultMeta};
use fsonos_api::events::{EventDto, EventsDto, MAX_WAIT, ResourceChanges};
use fsonos_api::off_thread::off_thread;
use fsonos_api::{ErrorCode, Failure};
use std::time::Duration;

use super::{Backend, backend_for, from_daemon, respond};
use crate::tool_error;

/// One line per event: its id, kind and details.
fn event_line(e: &EventDto) -> String {
    format!("#{} {} {}", e.id, e.event, e.data)
}

impl Backend {
    /// The resource publisher needs only journal continuity, never event
    /// payloads or an invented client identity. A stdio bridge still asks the
    /// selected daemon with its existing authenticated policy identity.
    pub(crate) fn resource_changes_answer(
        &self,
        since: Option<u64>,
        wait_secs: u64,
    ) -> Result<ResourceChanges, Failure> {
        if self.daemon.is_some() {
            return self
                .events_answer(since, wait_secs)
                .map(|answer| ResourceChanges {
                    last_id: answer.last_id,
                    reset: answer.reset,
                });
        }
        self.surface
            .resource_changes_after(since, Duration::from_secs(wait_secs))
    }

    /// The `events` tool. The stream is the daemon's: from the stdio server,
    /// the running daemon answers (`GET /events?wait=`).
    pub fn events(&self, since: Option<u64>, wait_secs: u64) -> McpResult<FinalCallToolResult> {
        Self::events_said(self.events_answer(since, wait_secs))
    }

    fn events_said(answer: Result<EventsDto, Failure>) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let answer = answer?;
            let mut lines: Vec<String> = answer.events.iter().map(event_line).collect();
            if let Some(oldest) = answer.reset {
                lines.insert(
                    0,
                    format!("Events before #{oldest} are gone: read list_zones afresh."),
                );
            }
            lines.push(format!(
                "Newest event: #{} (pass it as since).",
                answer.last_id
            ));
            Ok((lines.join("\n"), answer))
        })
    }

    pub(crate) fn events_answer(
        &self,
        since: Option<u64>,
        wait_secs: u64,
    ) -> Result<EventsDto, Failure> {
        if let Some(daemon) = &self.daemon {
            let path = match since {
                Some(since) => format!("/events?wait={wait_secs}&since={since}"),
                None => format!("/events?wait={wait_secs}"),
            };
            if let Some(answer) = daemon.call("GET", &path, None) {
                return from_daemon(answer, "GET", &path);
            }
        }
        self.surface
            .events_after(&self.client, since, Duration::from_secs(wait_secs))
    }
}

#[tool(
    description = "What changed in the house since an event id: playback and track changes, volume and mute, regroups, players going offline, logged actions (who did what), and the DJ's picks. Call it without `since` to get the newest event id, then with `since` (the last_id you got) and `wait_secs` (0-60, default 0) to wait for the next changes; each answer has a new last_id. A `reset` means events were missed: read list_zones afresh. Needs the daemon (`fsonos serve`).",
    annotations(read_only)
)]
async fn events(
    ctx: &McpContext,
    since: Option<u64>,
    wait_secs: Option<u64>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    ctx.checkpoint()?;
    let b = backend_for(ctx).map_err(|f| tool_error(&f))?;
    let wait_secs = wait_secs.unwrap_or(0).min(MAX_WAIT.as_secs());
    let answer = match off_thread(move || b.events_answer(since, wait_secs)) {
        Ok(waiting) => waiting.await.unwrap_or_else(|| {
            Err(Failure::new(
                ErrorCode::Internal,
                "no thread to wait for events on",
            ))
        }),
        Err(busy) => Err(busy),
    };
    Ok(CompleteResult::new(
        Backend::events_said(answer)?,
        ResultMeta::empty(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{Daemon, Survey};
    use fsonos_core::clock::SystemClock;
    use fsonos_core::policy::{Client, Policy};
    use fsonos_proto::{ProtoError, Transport};
    use std::sync::{Arc, Mutex};

    struct NoSpeakers;

    impl Transport for NoSpeakers {
        fn soap_post(
            &self,
            host: std::net::IpAddr,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<String, ProtoError> {
            Err(ProtoError::Network {
                target: host.to_string(),
                detail: "no speakers here".into(),
            })
        }
    }

    /// A daemon with one zone.state event after 7, recording what it is asked.
    struct Running(Mutex<Vec<String>>);

    impl Daemon for Running {
        fn call(
            &self,
            method: &str,
            path: &str,
            _: Option<&serde_json::Value>,
        ) -> Option<Result<serde_json::Value, Failure>> {
            self.0.lock().unwrap().push(format!("{method} {path}"));
            Some(Ok(serde_json::json!({
                "events": [{"id": 8, "event": "zone.state", "data": {"room": "Den", "volume": 30}}],
                "last_id": 8
            })))
        }
    }

    fn backend() -> Backend {
        let survey: Survey = Box::new(|_| Ok(Vec::new()));
        Backend::new(
            Box::new(NoSpeakers),
            survey,
            Policy::default(),
            Client::McpStdio,
            Box::new(SystemClock),
        )
    }

    #[test]
    fn the_stdio_server_asks_the_running_daemon() {
        let daemon = Arc::new(Running(Mutex::new(Vec::new())));
        let b = backend().with_daemon(Arc::clone(&daemon) as Arc<dyn Daemon>);
        let r = b.events(Some(7), 30).unwrap();
        assert!(!r.is_error);
        assert_eq!(*daemon.0.lock().unwrap(), ["GET /events?wait=30&since=7"]);
        let answer = r.structured_content.unwrap();
        assert_eq!(answer["last_id"], 8);
        assert_eq!(answer["events"][0]["event"], "zone.state");
    }

    #[test]
    fn without_the_daemon_there_is_no_stream() {
        let err = backend().events(None, 0).unwrap_err();
        assert!(format!("{err:?}").contains("NOT_IMPLEMENTED"), "{err:?}");
    }
}
