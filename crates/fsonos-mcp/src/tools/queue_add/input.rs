//! Queue additions share the running daemon's inventory, policy and undo log.
//! Once a daemon answers, no error authorizes a second local mutation.

use fastmcp::FinalCallToolResult;
use fastmcp::prelude::*;
use fsonos_api::surface::queue::{QueueAddOutcome, QueueRequest};
use fsonos_api::{ErrorCode, Failure};

use super::super::{Backend, from_daemon, respond};

impl Backend {
    /// Preserve every target receipt and expose incomplete delivery as a tool
    /// error in both MCP eras. A selected daemon remains the only authority.
    pub(in crate::tools) fn write_queue<R: serde::Serialize>(
        &self,
        path: &str,
        request: &R,
        here: impl FnOnce() -> Result<QueueAddOutcome, Failure>,
    ) -> McpResult<FinalCallToolResult> {
        let body = serde_json::to_value(request).map_err(|error| {
            crate::tool_error(&Failure::new(ErrorCode::Internal, error.to_string()))
        })?;
        let mut complete = false;
        let mut result = respond(|| {
            let added = match self
                .daemon
                .as_ref()
                .and_then(|daemon| daemon.call("POST", path, Some(&body)))
            {
                Some(answer) => from_daemon::<QueueAddOutcome>(answer, "POST", path)?,
                None => here()?,
            };
            complete = match &added {
                QueueAddOutcome::Single(_) => true,
                QueueAddOutcome::Groups(batch) => {
                    batch.complete && batch.failed.is_empty() && !batch.added.is_empty()
                }
            };
            Ok((added.text(), added))
        })?;
        result.is_error = !complete;
        Ok(result)
    }

    /// Queue a Spotify source, native favorite or saved Sonos playlist on the daemon
    /// when present. The original `add_to_queue` Rust helper delegates here.
    pub fn queue_request(&self, req: &QueueRequest) -> McpResult<FinalCallToolResult> {
        req.validate()
            .map_err(|failure| crate::tool_error(&failure))?;
        self.write_queue("/queue", req, || {
            self.surface.queue_request_targets(&self.client, req)
        })
    }
}

#[cfg(test)]
mod tests;
