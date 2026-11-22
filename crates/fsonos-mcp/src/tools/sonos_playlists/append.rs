//! Saved-playlist composition forwards the reviewed source and destination once.

use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult};
use fsonos_api::surface::sonos_playlists::{
    AppendSonosPlaylistTracksRequest, AppendedSonosPlaylistDto,
};
use fsonos_api::{ErrorCode, Failure};

use super::{Backend, respond, with_backend_off_thread};

impl Backend {
    pub fn append_sonos_playlist_tracks(
        &self,
        req: &AppendSonosPlaylistTracksRequest,
    ) -> McpResult<FinalCallToolResult> {
        req.validate()
            .map_err(|failure| crate::tool_error(&failure))?;
        let body = serde_json::to_value(req).map_err(|error| {
            crate::tool_error(&Failure::new(ErrorCode::Internal, error.to_string()))
        })?;
        if let Some(result) = self.on_daemon::<AppendedSonosPlaylistDto>(
            "POST",
            "/sonos/playlists/append-tracks",
            Some(&body),
            AppendedSonosPlaylistDto::text,
        ) {
            return result;
        }
        respond(|| {
            let result = self
                .surface
                .append_sonos_playlist_tracks(&self.client, req)?;
            Ok((result.text(), result))
        })
    }
}

#[tool(
    description = "Append 1-10 reviewed track occurrences to an existing native Sonos saved playlist, preserving the supplied order and repeated positions. First read sonos_playlist_contents for the destination and pass its exact playlist.id (SQ:n) and revision as expected_revision. By default the source is zone's current group queue: read get_queue and pass its revision as source_revision with one-based positions. To copy from a saved playlist in the same household, set source_playlist to its exact SQ: id and use that playlist's contents revision as source_revision. Both revisions are required. zone accepts Room@S1 or Room@S2 to disambiguate households. The server resolves native resources and metadata from the reviewed occurrences; it never accepts client media URIs or metadata. Only the destination saved playlist changes; loaded queues, playback, volume and DJ stay unchanged. Returns the destination playlist, previous_total, total, added count, selected positions/titles and a new revision. Uses append_sonos_playlist_tracks / house-config permission; no Spotify sign-in is needed. The saved edit is not undoable through action history. Never automatically retry an uncertain or malformed reply: read the destination saved contents on the original household before another append."
)]
async fn append_sonos_playlist_tracks(
    ctx: &McpContext,
    zone: String,
    playlist: String,
    expected_revision: String,
    source_revision: String,
    positions: Vec<u32>,
    source_playlist: Option<String>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend_off_thread(ctx, move |backend| {
        backend.append_sonos_playlist_tracks(&AppendSonosPlaylistTracksRequest {
            zone,
            playlist,
            expected_revision,
            source_playlist,
            source_revision,
            positions,
        })
    })
    .await
}
