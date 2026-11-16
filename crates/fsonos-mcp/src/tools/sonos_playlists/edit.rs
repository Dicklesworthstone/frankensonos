//! Saved occurrence edits forward once to the chosen daemon with the proof intact.

use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult};
use fsonos_api::surface::sonos_playlists::{
    EditedSonosPlaylistDto, MoveSonosPlaylistTracksRequest, RemoveSonosPlaylistTracksRequest,
};
use fsonos_api::{ErrorCode, Failure};

use super::{Backend, respond, with_backend_off_thread};

impl Backend {
    pub fn remove_sonos_playlist_tracks(
        &self,
        req: &RemoveSonosPlaylistTracksRequest,
    ) -> McpResult<FinalCallToolResult> {
        req.validate()
            .map_err(|failure| crate::tool_error(&failure))?;
        let body = serde_json::to_value(req).map_err(|error| {
            crate::tool_error(&Failure::new(ErrorCode::Internal, error.to_string()))
        })?;
        if let Some(result) = self.on_daemon::<EditedSonosPlaylistDto>(
            "POST",
            "/sonos/playlists/remove-tracks",
            Some(&body),
            EditedSonosPlaylistDto::text,
        ) {
            return result;
        }
        respond(|| {
            let result = self
                .surface
                .remove_sonos_playlist_tracks(&self.client, req)?;
            Ok((result.text(), result))
        })
    }

    pub fn move_sonos_playlist_tracks(
        &self,
        req: &MoveSonosPlaylistTracksRequest,
    ) -> McpResult<FinalCallToolResult> {
        req.validate()
            .map_err(|failure| crate::tool_error(&failure))?;
        let body = serde_json::to_value(req).map_err(|error| {
            crate::tool_error(&Failure::new(ErrorCode::Internal, error.to_string()))
        })?;
        if let Some(result) = self.on_daemon::<EditedSonosPlaylistDto>(
            "POST",
            "/sonos/playlists/move-tracks",
            Some(&body),
            EditedSonosPlaylistDto::text,
        ) {
            return result;
        }
        respond(|| {
            let result = self.surface.move_sonos_playlist_tracks(&self.client, req)?;
            Ok((result.text(), result))
        })
    }
}

#[tool(
    description = "Permanently remove consecutive track occurrences from one native Sonos saved playlist while leaving all loaded playing queues, playback, volume and DJ unchanged. First read sonos_playlist_contents and use its exact playlist.id (SQ:n) and revision as expected_revision on the same household/group. position is the first original one-based occurrence and count is the positive number of consecutive entries. Duplicate URIs remain distinct occurrences. The complete saved playlist must fit 1000 entries and 4 MiB for full verification. A changed playlist or group is refused. A confirmed receipt includes previous_total, total, and the new contents revision. Removing the last entries keeps the empty saved playlist. Uses remove_sonos_playlist_tracks / house-config permission; no Spotify sign-in is needed. Not undoable through action history. An uncertain or malformed response is never replayed automatically: read saved contents on the original household before another edit."
)]
async fn remove_sonos_playlist_tracks(
    ctx: &McpContext,
    zone: String,
    playlist: String,
    expected_revision: String,
    position: u32,
    count: u32,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend_off_thread(ctx, move |backend| {
        backend.remove_sonos_playlist_tracks(&RemoveSonosPlaylistTracksRequest {
            zone,
            playlist,
            expected_revision,
            position,
            count,
        })
    })
    .await
}

#[tool(
    description = "Move a contiguous block within one native Sonos saved playlist, retaining duplicate occurrences and the block's internal order. First read sonos_playlist_contents, then use its exact playlist.id (SQ:n), revision as expected_revision and same household/group. position and insert_before both refer to the ORIGINAL one-based contents; count is positive. Original total + 1 appends. An insertion slot in the selected block or immediately after it is a verified no-op. The complete playlist must fit 1000 entries and 4 MiB for full verification. Returns the original selected range, previous_total, total and observed new revision. Existing playing queues, playback, volume and DJ are untouched. Uses move_sonos_playlist_tracks / house-config permission; no Spotify sign-in is needed. Not undoable through action history. An uncertain or malformed response is never replayed automatically: read saved contents on the original household before another edit."
)]
async fn move_sonos_playlist_tracks(
    ctx: &McpContext,
    zone: String,
    playlist: String,
    expected_revision: String,
    position: u32,
    count: u32,
    insert_before: u32,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend_off_thread(ctx, move |backend| {
        backend.move_sonos_playlist_tracks(&MoveSonosPlaylistTracksRequest {
            zone,
            playlist,
            expected_revision,
            position,
            count,
            insert_before,
        })
    })
    .await
}
