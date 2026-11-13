//! Queue additions share the running daemon's inventory, policy and undo log.
//! Once a daemon answers, no error authorizes a second local mutation.

use fastmcp::FinalCallToolResult;
use fastmcp::prelude::*;
use fsonos_api::surface::queue::{QueueAddOutcome, QueueRequest};
use fsonos_api::{ErrorCode, Failure};

use super::super::{Backend, respond};

impl Backend {
    /// Queue a Spotify source, native favorite or saved Sonos playlist on the daemon
    /// when present. The original `add_to_queue` Rust helper delegates here.
    pub fn queue_request(&self, req: &QueueRequest) -> McpResult<FinalCallToolResult> {
        req.validate()
            .map_err(|failure| crate::tool_error(&failure))?;
        let body = serde_json::to_value(req).map_err(|error| {
            crate::tool_error(&Failure::new(ErrorCode::Internal, error.to_string()))
        })?;
        if let Some(answer) =
            self.on_daemon::<QueueAddOutcome>("POST", "/queue", Some(&body), QueueAddOutcome::text)
        {
            return answer;
        }
        respond(|| {
            let added = self.surface.queue_request_targets(&self.client, req)?;
            Ok((added.text(), added))
        })
    }
}

#[cfg(test)]
mod tests;
