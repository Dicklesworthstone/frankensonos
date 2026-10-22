//! Bounded searches return exact queue occurrences for later guarded actions.

use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult};
use fsonos_api::surface::queue::QueueSearchRequest;

use super::{Backend, respond, with_backend_off_thread};

impl Backend {
    /// Search through the selected daemon or the shared standalone surface.
    /// Both paths authorize this read with the existing `get_queue` permission.
    pub fn search_queue(&self, request: &QueueSearchRequest) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let path = request.path()?;
            let found = self.read(&path, || self.surface.queue_search(&self.client, request))?;
            Ok((found.text(), found))
        })
    }
}

#[tool(
    description = "Find title, artist or album words in a room's existing group queue without changing playback or adding tracks. Matching ignores case and accents; every word must match. Returns exact one-based positions, renderer URIs and an opaque queue revision, keeping duplicate occurrences separate. limit is 1-200 matches (default 50); each call scans a bounded section, so inspect complete, examined and next_offset even if no matches return. To continue, pass next_offset as offset and the same revision as expected_revision, retaining zone and query. Offset above zero requires expected_revision. A changed queue fails instead of mixing revisions; begin a fresh search explicitly. To play a result, pass its position, exact URI as expected_uri and revision as expected_revision to play_queue. Search results are not necessarily consecutive; read get_queue before a range move or removal. Uses the get_queue read permission.",
    annotations(read_only, idempotent)
)]
async fn search_queue(
    ctx: &McpContext,
    zone: String,
    query: String,
    offset: Option<u32>,
    limit: Option<u32>,
    expected_revision: Option<String>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend_off_thread(ctx, move |backend| {
        backend.search_queue(&QueueSearchRequest {
            zone,
            query,
            offset: offset.unwrap_or(0),
            limit: limit.unwrap_or(50),
            expected_revision,
        })
    })
    .await
}

#[cfg(test)]
mod tests;
