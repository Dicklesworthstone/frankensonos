//! Queue or play one ordered, guarded selection of explicit Spotify sources.

use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult};
use fsonos_api::surface::queue::{
    PlayTracksRequest, QueueTrackInput, QueueTracksRequest,
};

use super::{Backend, respond, said, with_backend_off_thread};

/// MCP's schema generator is distinct from the HTTP OpenAPI generator. Keep
/// its typed input here, then use the shared request validation and execution.
#[derive(Debug, serde::Deserialize, fastmcp::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct SelectedTrack {
    source_uri: String,
    #[serde(default)]
    title: Option<String>,
}

impl From<SelectedTrack> for QueueTrackInput {
    fn from(track: SelectedTrack) -> Self {
        Self {
            source_uri: track.source_uri,
            title: track.title,
        }
    }
}

impl Backend {
    /// Prefer the selected daemon's playback boundary and action log. The
    /// shared typed write treats failed or malformed replies as terminal.
    pub fn play_tracks(&self, request: &PlayTracksRequest) -> McpResult<FinalCallToolResult> {
        request
            .validate()
            .map_err(|error| crate::tool_error(&error))?;
        self.write("/play/tracks", request, || {
            respond(|| self.surface.play_tracks(&self.client, request).map(said))
        })
    }

    pub fn add_tracks_to_queue(
        &self,
        request: &QueueTracksRequest,
    ) -> McpResult<FinalCallToolResult> {
        request
            .validate()
            .map_err(|error| crate::tool_error(&error))?;
        self.write_queue("/queue/tracks", request, || {
            self.surface.queue_tracks_targets(&self.client, request)
        })
    }
}

#[tool(
    description = "Replace each selected group's queue with the same ordered selection of 1-10 Spotify tracks or episodes and start the first occurrence in NORMAL mode (shuffle and repeat off). zone accepts a room, saved alias, here, all/everywhere, all@S1 or all@S2; Room@S1 or Room@S2 disambiguates households. Already grouped rooms are deduplicated and groups keep their members; playback across groups is independent, not synchronized. Each tracks entry has source_uri (a Spotify track/episode URI or share link) and optional title. Preserve the supplied order and intentional duplicates, including mixed tracks and episodes; albums, artists, playlists and shows are not expanded here. No Spotify Web API sign-in is needed for concrete track identities; the household still needs working Spotify playback linkage. Optional expected_revision is the destination queue revision from get_queue, not a cached library revision, and requires a selector resolving to one group; a changed queue is refused. Uses the existing play permission and room volume caps. One action is logged for the complete selection; undo_last can restore the previous queues and playback only when complete before-and-after proof was captured for every group. Multi-group receipts preserve each target's result; partial delivery is an error and must not be replayed as a batch. A failed or uncertain response is terminal: inspect playback, the queue and action log before another action; never automatically replay the selection."
)]
async fn play_tracks(
    ctx: &McpContext,
    zone: String,
    tracks: Vec<SelectedTrack>,
    expected_revision: Option<String>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    let request = PlayTracksRequest {
        zone,
        tracks: tracks.into_iter().map(QueueTrackInput::from).collect(),
        expected_revision,
    };
    with_backend_off_thread(ctx, move |backend| backend.play_tracks(&request)).await
}

#[tool(
    description = "Quietly add the same ordered selection of 1-10 Spotify tracks or episodes to every selected group's queue in one request. zone accepts a room, saved alias, here, all/everywhere, all@S1 or all@S2. Grouped rooms queue once and keep their current members. Each tracks entry has source_uri (a Spotify track/episode URI or share link) and optional title. Preserve the caller's order and intentional duplicates; albums, artists, playlists and shows are not accepted here. next=true inserts the entire selection after the current item when the queue is selected, otherwise it appends; neither mode starts playback or changes the selected source. Optional expected_revision must be the destination queue revision from get_queue, not a cached library revision, and requires a selector resolving to one group; a changed queue is refused. Uses the same add_to_queue permission. Single-group replies retain zone/first/count/playing_queue; multi-group receipts include complete, added and failed entries, logged and undoable. Incomplete delivery sets the tool-error flag while retaining every target receipt. Confirmed additions share one undo when every possible effect is proved; any uncertain insertion blocks batch undo. Inspect failed groups before another action; never automatically replay the whole selection."
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
        tracks: tracks.into_iter().map(QueueTrackInput::from).collect(),
        next: next.unwrap_or(false),
        expected_revision,
    };
    with_backend_off_thread(ctx, move |backend| backend.add_tracks_to_queue(&request)).await
}

#[cfg(test)]
mod tests;
