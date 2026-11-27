//! Native queue selections share the daemon's guarded playback and queue paths.

use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult};
use fsonos_api::surface::queue::{QueueSelectionDto, QueueSelectionRequest};
use fsonos_api::{ErrorCode, Failure};

use super::{Backend, respond, with_backend_off_thread};

fn outcome(mut result: FinalCallToolResult) -> FinalCallToolResult {
    if result
        .structured_content
        .as_ref()
        .and_then(|value| serde_json::from_value::<QueueSelectionDto>(value.clone()).ok())
        .is_some_and(|selection| !selection.complete())
    {
        result.is_error = true;
    }
    result
}

impl Backend {
    pub fn queue_selection(
        &self,
        request: &QueueSelectionRequest,
    ) -> McpResult<FinalCallToolResult> {
        request
            .validate()
            .map_err(|failure| crate::tool_error(&failure))?;
        let body = serde_json::to_value(request).map_err(|error| {
            crate::tool_error(&Failure::new(ErrorCode::Internal, error.to_string()))
        })?;
        if let Some(result) = self.on_daemon::<QueueSelectionDto>(
            "POST",
            "/queue/selection",
            Some(&body),
            QueueSelectionDto::text,
        ) {
            return result.map(outcome);
        }
        respond(|| {
            let selected = self.surface.queue_selection(&self.client, request)?;
            Ok((selected.text(), selected))
        })
        .map(outcome)
    }
}

#[tool(
    description = "Play or queue 1-10 exact native occurrences from an observed live Sonos queue, preserving their order and intentional repeats. First read get_queue or search_queue; keep zone as that source group, positions as its one-based positions, and expected_revision as its queue revision. Optional target names another room, saved alias or all@S1/all@S2 in the SAME household; omit it to target the source group itself. Already grouped rooms receive one copy. mode is required: play stages and verifies the selection before replacing each destination queue and starts its first occurrence in NORMAL order; next inserts after the current item when its queue is selected, otherwise appends; append adds at the end. Next and append preserve playback and volume. The source queue and playback remain intact unless its group is targeted; a source that is also a destination runs last so every other group uses the original source occurrences. Native resource and service metadata are read from Sonos, never supplied by the client; local files and non-Spotify services are supported without Spotify Web API sign-in. expected_target_revision optionally binds the destination audience from resolve_rooms. expected_queue_revision optionally binds one destination queue and cannot be used for a multi-group set. Uses play_favorite permission and volume caps for play, add_to_queue for next/append. One action records confirmed effects and undo when ownership is proved. batch or queue_batch preserves each target result; incomplete delivery is a tool error with receipts. Never retry a failed or uncertain selection automatically; read the queues and action log before another command."
)]
async fn queue_selection(
    ctx: &McpContext,
    zone: String,
    positions: Vec<u32>,
    expected_revision: String,
    mode: String,
    target: Option<String>,
    expected_target_revision: Option<String>,
    expected_queue_revision: Option<String>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    let mode = mode
        .parse()
        .map_err(|failure| crate::tool_error(&failure))?;
    with_backend_off_thread(ctx, move |backend| {
        backend.queue_selection(&QueueSelectionRequest {
            zone,
            positions,
            expected_revision,
            mode,
            target,
            expected_target_revision,
            expected_queue_revision,
        })
    })
    .await
}

#[cfg(test)]
mod tests;
