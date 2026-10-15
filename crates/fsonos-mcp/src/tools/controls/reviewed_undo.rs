//! Agent-native recovery through the same action log that runs the controls.

use crate::tools::{Backend, from_daemon, respond};
use fastmcp::FinalCallToolResult;
use fastmcp::prelude::*;
use fsonos_api::{ActionDto, ErrorCode, Failure, UndoDto};
use serde::Serialize;
use serde::de::DeserializeOwned;

#[derive(Serialize)]
struct PreviewDto {
    action: Option<ActionDto>,
}

fn undo_text(undone: &UndoDto) -> String {
    std::iter::once(undone.summary.clone())
        .chain(undone.failures.iter().map(|why| format!("Not restored: {why}")))
        .collect::<Vec<_>>()
        .join("\n")
}

impl Backend {
    /// Log identities are local to their store. Once a daemon is configured,
    /// reviewed undo must not fall through to a different process's log,
    /// even if both logs happen to contain the same numeric action id.
    fn on_log_authority<T: DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<Option<T>, Failure> {
        let Some(daemon) = &self.daemon else {
            return Ok(None);
        };
        let answer = daemon.call(method, path, body).ok_or_else(|| {
            Failure::new(ErrorCode::NotReady, "the daemon's action log is not reachable")
                .with_hint("Reconnect to the same daemon; no local log or alternate undo was used.")
        })?;
        from_daemon(answer, method, path).map(Some).map_err(|failure| {
            if method != "GET" && failure.code == ErrorCode::Internal {
                failure.with_hint(
                    "Undo may be uncertain; inspect playback and preview again without automatic replay.",
                )
            } else {
                failure
            }
        })
    }

    /// Preview does not infer eligibility from a bounded history window and
    /// does not acquire permission to undo. The surface remains authoritative.
    pub fn preview_undo(&self) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let selected =
                self.on_log_authority::<Option<ActionDto>>("GET", "/undo/preview", None)?;
            let action = match selected {
                Some(action) => action,
                None => self.surface.preview_undo(&self.client)?,
            };
            if action.as_ref().is_some_and(|a| {
                a.id <= 0 || !a.undoable || a.undo_of.is_some() || a.client != self.client.key()
            }) {
                return Err(Failure::new(
                    ErrorCode::Internal,
                    "the undo preview is inconsistent",
                )
                .with_hint("Read a new preview from the same daemon; nothing was restored."));
            }
            let text = action.as_ref().map_or_else(
                || "nothing to undo".to_string(),
                |a| format!(
                    "Review action #{} by {}: {}\nResult: {}\n\
                     Confirm with undo_checked(expected_id={}). Restoring its before-state \
                     can overwrite later changes; there is no redo.",
                    a.id, a.client, a.intent, a.result, a.id,
                ),
            );
            Ok((text, PreviewDto { action }))
        })
    }

    /// Never retry or widen to legacy undo-last when a reviewed id is refused.
    pub fn undo_checked(&self, expected_id: i64) -> McpResult<FinalCallToolResult> {
        respond(|| {
            if expected_id <= 0 {
                return Err(Failure::invalid("expected_id must be a positive action id"));
            }
            let body = serde_json::json!({ "expected_id": expected_id });
            let result = self.on_log_authority::<UndoDto>("POST", "/undo/checked", Some(&body))?;
            let undone = match result {
                Some(undone) => undone,
                None => self.surface.undo_checked(&self.client, expected_id)?,
            };
            if undone.undone != Some(expected_id) {
                return Err(Failure::new(
                    ErrorCode::Internal,
                    "the undo reply did not confirm the reviewed action; the outcome is uncertain",
                )
                .with_hint("Inspect playback and read a new preview; do not automatically retry undo."));
            }
            Ok((undo_text(&undone), undone))
        })
    }
}

#[cfg(test)]
mod tests;
