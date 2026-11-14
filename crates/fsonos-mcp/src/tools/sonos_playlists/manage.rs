//! Catalog mutations use one selected-daemon request with no local replay.

use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult};
use fsonos_api::surface::sonos_playlists::{
    ManagedSonosPlaylistDto, RemoveSonosPlaylistRequest, RenameSonosPlaylistRequest,
};
use fsonos_api::{ErrorCode, Failure};

use super::{Backend, respond, with_backend_off_thread};

impl Backend {
    pub fn rename_sonos_playlist(
        &self,
        req: &RenameSonosPlaylistRequest,
    ) -> McpResult<FinalCallToolResult> {
        req.validate()
            .map_err(|failure| crate::tool_error(&failure))?;
        let body = serde_json::to_value(req).map_err(|error| {
            crate::tool_error(&Failure::new(ErrorCode::Internal, error.to_string()))
        })?;
        if let Some(result) = self.on_daemon::<ManagedSonosPlaylistDto>(
            "POST",
            "/sonos/playlists/rename",
            Some(&body),
            ManagedSonosPlaylistDto::text,
        ) {
            return result;
        }
        respond(|| {
            let result = self.surface.rename_sonos_playlist(&self.client, req)?;
            Ok((result.text(), result))
        })
    }

    pub fn remove_sonos_playlist(
        &self,
        req: &RemoveSonosPlaylistRequest,
    ) -> McpResult<FinalCallToolResult> {
        req.validate()
            .map_err(|failure| crate::tool_error(&failure))?;
        let body = serde_json::to_value(req).map_err(|error| {
            crate::tool_error(&Failure::new(ErrorCode::Internal, error.to_string()))
        })?;
        if let Some(result) = self.on_daemon::<ManagedSonosPlaylistDto>(
            "POST",
            "/sonos/playlists/remove",
            Some(&body),
            ManagedSonosPlaylistDto::text,
        ) {
            return result;
        }
        respond(|| {
            let result = self.surface.remove_sonos_playlist(&self.client, req)?;
            Ok((result.text(), result))
        })
    }
}

#[tool(
    description = "Rename one native Sonos saved playlist without changing its identity, contents, playing queue, playback, volume or DJ. First read sonos_playlist_contents for this household/group, then supply its exact playlist.id (SQ:n) and revision as expected_revision. Titles and catalog numbers cannot identify the target. title must be nonblank, contain no control characters and fit 1023 UTF-8 bytes after trimming. Changed contents, player or group are refused before the write. A fresh catalog confirms the new title; a same-title request is a validated no-op. Uses rename_sonos_playlist / house-config permission. Not undoable through action history. Never automatically retry an uncertain response: read list_sonos_playlists on the original household before any new edit."
)]
async fn rename_sonos_playlist(
    ctx: &McpContext,
    zone: String,
    playlist: String,
    expected_revision: String,
    title: String,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend_off_thread(ctx, move |backend| {
        backend.rename_sonos_playlist(&RenameSonosPlaylistRequest {
            zone,
            playlist,
            expected_revision,
            title,
        })
    })
    .await
}

#[tool(
    description = "Permanently remove one native Sonos saved playlist from its household. This removes its saved catalog entry, leaving any already loaded queue, playback, volume and DJ unchanged. Review the playlist with sonos_playlist_contents and pass its exact playlist.id (SQ:n) and revision as expected_revision on the same household/group; names and catalog numbers are not accepted. Changed contents, player or group are refused before the write. A fresh catalog must confirm absence. Uses remove_sonos_playlist / house-config permission. Cannot be undone through action history. Never automatically retry an uncertain response: inspect list_sonos_playlists on the original household before another edit."
)]
async fn remove_sonos_playlist(
    ctx: &McpContext,
    zone: String,
    playlist: String,
    expected_revision: String,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend_off_thread(ctx, move |backend| {
        backend.remove_sonos_playlist(&RemoveSonosPlaylistRequest {
            zone,
            playlist,
            expected_revision,
        })
    })
    .await
}
