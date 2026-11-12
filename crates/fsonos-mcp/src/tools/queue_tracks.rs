//! One ordered, guarded batch from the library or explicit Spotify sources.

use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult};
use fsonos_api::surface::queue::{QueueAddedDto, QueueTrackInput, QueueTracksRequest};
use fsonos_api::{ErrorCode, Failure};

use super::{Backend, respond, with_backend_off_thread};

/// MCP's schema generator is distinct from the HTTP OpenAPI generator. Keep
/// its typed input here, then use the shared request validation and execution.
#[derive(Debug, serde::Deserialize, fastmcp::JsonSchema)]
#[serde(deny_unknown_fields)]
struct SelectedTrack {
    source_uri: String,
    #[serde(default)]
    title: Option<String>,
}

impl Backend {
    pub fn add_tracks_to_queue(
        &self,
        request: &QueueTracksRequest,
    ) -> McpResult<FinalCallToolResult> {
        request
            .validate()
            .map_err(|error| crate::tool_error(&error))?;
        let body = serde_json::to_value(request).map_err(|error| {
            crate::tool_error(&Failure::new(ErrorCode::Internal, error.to_string()))
        })?;
        if let Some(answer) = self.on_daemon(
            "POST",
            "/queue/tracks",
            Some(&body),
            |added: &QueueAddedDto| added.done.clone(),
        ) {
            return answer;
        }
        respond(|| {
            let added = self.surface.queue_tracks(&self.client, request)?;
            Ok((added.done.clone(), added))
        })
    }
}

#[tool(
    description = "Quietly append an ordered selection of 1-10 Spotify tracks or episodes to a room's group queue in one request. Each tracks entry has source_uri (a Spotify track/episode URI or share link) and optional title. Preserve the caller's order and intentional duplicates; albums, artists, playlists and shows are not accepted here. next=true inserts the entire selection after the current item when the queue is selected, otherwise it appends; neither mode starts playback or changes the selected source. Optional expected_revision must be the destination queue revision from get_queue, not a cached library revision; a changed queue is refused. Uses the same add_to_queue permission and successful additions can be undone with undo_last. On an uncertain failure, inspect the queue and action log before another action; never automatically replay the selection."
)]
async fn add_tracks_to_queue(
    ctx: &McpContext,
    zone: String,
    tracks: Vec<SelectedTrack>,
    next: Option<bool>,
    expected_revision: Option<String>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    let request = QueueTracksRequest {
        zone,
        tracks: tracks
            .into_iter()
            .map(|track| QueueTrackInput {
                source_uri: track.source_uri,
                title: track.title,
            })
            .collect(),
        next: next.unwrap_or(false),
        expected_revision,
    };
    with_backend_off_thread(ctx, move |backend| backend.add_tracks_to_queue(&request)).await
}

#[cfg(test)]
mod tests;
