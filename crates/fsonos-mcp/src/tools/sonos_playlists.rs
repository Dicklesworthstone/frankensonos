//! Native Sonos playlists use the shared household metadata and control path.

use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult};
use fsonos_api::surface::sonos_playlists::{self, PlaySonosPlaylistRequest, SonosPlaylistDto};
use serde::Serialize;

use super::{Backend, respond, said, segment, with_backend_off_thread};

#[derive(Serialize)]
struct PlaylistsDto {
    playlists: Vec<SonosPlaylistDto>,
}

impl Backend {
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

#[cfg(test)]
mod tests;
