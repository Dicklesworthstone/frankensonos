//! Native Sonos playlists use the shared household metadata and control path.

use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult};
use fsonos_api::surface::sonos_playlists::{
    self, PlaySonosPlaylistRequest, SaveSonosPlaylistRequest, SavedSonosPlaylistDto,
    SonosPlaylistContentsRequest, SonosPlaylistDto,
};
use fsonos_api::{ErrorCode, Failure};
use serde::Serialize;

use super::{Backend, respond, said, segment, with_backend_off_thread};

#[derive(Serialize)]
struct PlaylistsDto {
    playlists: Vec<SonosPlaylistDto>,
}

impl Backend {
    /// Inspect the selected daemon's saved playlist, preserving the page's
    /// native identity and revision for explicit continuation.
    pub fn sonos_playlist_contents(
        &self,
        req: &SonosPlaylistContentsRequest,
    ) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let path = req.path()?;
            let contents = self.read(&path, || {
                self.surface.sonos_playlist_contents(&self.client, req)
            })?;
            Ok((contents.text(), contents))
        })
    }

    /// Read the selected daemon's household, never a second local catalog.
    pub fn list_sonos_playlists(&self, zone: &str) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let path = format!("/sonos/playlists?zone={}", segment(zone));
            let playlists =
                self.read(&path, || self.surface.sonos_playlists(&self.client, zone))?;
            Ok((
                sonos_playlists::text(&playlists),
                PlaylistsDto { playlists },
            ))
        })
    }

    /// Forward one validated request; failure never causes a local replay.
    pub fn play_sonos_playlist(
        &self,
        req: &PlaySonosPlaylistRequest,
    ) -> McpResult<FinalCallToolResult> {
        req.validate()
            .map_err(|failure| crate::tool_error(&failure))?;
        self.write("/sonos/playlists/play", req, || {
            respond(|| {
                self.surface
                    .play_sonos_playlist(&self.client, req)
                    .map(said)
            })
        })
    }

    /// Create on the selected daemon once; an uncertain or malformed reply
    /// cannot cause a second save through the local speakers.
    pub fn save_sonos_playlist(
        &self,
        req: &SaveSonosPlaylistRequest,
    ) -> McpResult<FinalCallToolResult> {
        req.validate()
            .map_err(|failure| crate::tool_error(&failure))?;
        let body = serde_json::to_value(req).map_err(|error| {
            crate::tool_error(&Failure::new(ErrorCode::Internal, error.to_string()))
        })?;
        if let Some(result) = self.on_daemon::<SavedSonosPlaylistDto>(
            "POST",
            "/sonos/playlists/save",
            Some(&body),
            SavedSonosPlaylistDto::text,
        ) {
            return result;
        }
        respond(|| {
            let saved = self.surface.save_sonos_playlist(&self.client, req)?;
            Ok((saved.text(), saved))
        })
    }
}

#[tool(
    description = "List playlists saved on a Sonos household, independently of My Sonos favorites and Spotify's library. zone is any room in that household; Room@S1 or Room@S2 disambiguates equal room names. Each result has its household-local SQ: id and title. Play it with play_sonos_playlist, or append/insert it with add_to_queue's playlist argument. Reads the selected daemon and uses the existing list_favorites permission. No Spotify library sign-in is needed.",
    annotations(read_only, idempotent)
)]
async fn list_sonos_playlists(
    ctx: &McpContext,
    zone: String,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend_off_thread(ctx, move |backend| backend.list_sonos_playlists(&zone)).await
}

#[tool(
    description = "Inspect a playlist saved on a Sonos household without replacing the playing queue or changing playback, volume or the DJ. This reads native SQ: contents, not Spotify's cached playlists, and needs no Spotify sign-in. Give zone (Room@S1 or Room@S2 disambiguates households) and playlist as an SQ: id, current catalog number or unambiguous title from list_sonos_playlists. Returns title, artist, album, available duration/art/renderer identity, and exact one-based positions; duplicate tracks remain separate occurrences. limit is 1-100 (default 100). Continue with next_offset, the returned playlist.id and revision as expected_revision. An offset above zero requires the revision; a changed playlist, player or group fails instead of mixing pages. Start a fresh read explicitly after change. Uses list_favorites read permission. Rows are inspection only; play_sonos_playlist or add_to_queue still select the whole native playlist.",
    annotations(read_only, idempotent)
)]
async fn sonos_playlist_contents(
    ctx: &McpContext,
    zone: String,
    playlist: String,
    offset: Option<u32>,
    limit: Option<u32>,
    expected_revision: Option<String>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend_off_thread(ctx, move |backend| {
        backend.sonos_playlist_contents(&SonosPlaylistContentsRequest {
            zone,
            playlist,
            offset: offset.unwrap_or(0),
            limit: limit.unwrap_or(100),
            expected_revision,
        })
    })
    .await
}

#[tool(
    description = "Replace a room's group queue with a playlist saved on its Sonos household and start it. Read list_sonos_playlists first, then pass playlist as its SQ: id, numbered position or unambiguous title. Metadata comes from that household, not from the caller, and no Spotify library sign-in is needed. Uses play_favorite permission, volume caps, staged queue replacement and action logging. Existing queue contents are replaced; use add_to_queue with playlist to append/insert without replacing or starting playback. Never automatically retry an uncertain response."
)]
async fn play_sonos_playlist(
    ctx: &McpContext,
    zone: String,
    playlist: String,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend_off_thread(ctx, move |backend| {
        backend.play_sonos_playlist(&PlaySonosPlaylistRequest { zone, playlist })
    })
    .await
}

#[tool(
    description = "Save the current queue of a room's group as a new playlist on its Sonos household. Give zone (Room@S1 or Room@S2 disambiguates names) and title (nonblank, no control characters, at most 1023 UTF-8 bytes after trimming). The queue must contain tracks. Always creates a new native SQ: id; it cannot overwrite or delete a playlist. Does not start, stop or change playback, queue contents, volume or the DJ. Returns the playlist confirmed in that household's catalog and whether the separate action log was saved. Uses save_sonos_playlist / house-config permission; no Spotify sign-in is needed. Not undoable through action history. Never automatically retry an uncertain response: read list_sonos_playlists for this household first."
)]
async fn save_sonos_playlist(
    ctx: &McpContext,
    zone: String,
    title: String,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend_off_thread(ctx, move |backend| {
        backend.save_sonos_playlist(&SaveSonosPlaylistRequest { zone, title })
    })
    .await
}

#[cfg(test)]
mod tests;
