//! Direct saved-playlist curation keeps current playback and queues intact.

use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult};
use fsonos_api::surface::queue::QueueTrackInput;
use fsonos_api::surface::sonos_playlists::{
    AddTracksToSonosPlaylistRequest, AddedTracksToSonosPlaylistDto,
};
use fsonos_api::{ErrorCode, Failure};

use super::super::queue_tracks::SelectedTrack;
use super::{Backend, respond, with_backend_off_thread};

impl Backend {
    pub fn add_tracks_to_sonos_playlist(
        &self,
        request: &AddTracksToSonosPlaylistRequest,
    ) -> McpResult<FinalCallToolResult> {
        request
            .validate()
            .map_err(|failure| crate::tool_error(&failure))?;
        let body = serde_json::to_value(request).map_err(|error| {
            crate::tool_error(&Failure::new(ErrorCode::Internal, error.to_string()))
        })?;
        if let Some(result) = self.on_daemon::<AddedTracksToSonosPlaylistDto>(
            "POST",
            "/sonos/playlists/add-tracks",
            Some(&body),
            AddedTracksToSonosPlaylistDto::text,
        ) {
            return result;
        }
        respond(|| {
            let result = self
                .surface
                .add_tracks_to_sonos_playlist(&self.client, request)?;
            Ok((result.text(), result))
        })
    }
}

#[tool(
    description = "Add an ordered selection of 1-10 Spotify tracks or episodes directly to an existing native Sonos saved playlist. First read sonos_playlist_contents on the intended household and pass its exact playlist.id (SQ:n) and revision as expected_revision. zone names one room on that household; Room@S1 or Room@S2 disambiguates households. Each tracks entry has source_uri (a Spotify track/episode URI or share link) and optional title. Preserve the supplied order and intentional duplicates. Albums, artists, playlists and shows are not expanded. The server learns the destination household's Spotify renderer; no Spotify Web API sign-in is required, but Sonos must have working Spotify linkage. Only the saved playlist changes: current playback, loaded queues, volume, grouping and DJ ownership stay unchanged. Returns the destination playlist, previous_total, total, added count, canonical source_uris, titles, new revision and logging status. Uses add_tracks_to_sonos_playlist / house-config permission. The saved edit has no action-history undo. A partial batch or uncertain reply must never be replayed automatically; inspect the destination saved contents on the original household before another append."
)]
async fn add_tracks_to_sonos_playlist(
    ctx: &McpContext,
    zone: String,
    playlist: String,
    expected_revision: String,
    tracks: Vec<SelectedTrack>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    let request = AddTracksToSonosPlaylistRequest {
        zone,
        playlist,
        expected_revision,
        tracks: tracks.into_iter().map(QueueTrackInput::from).collect(),
    };
    with_backend_off_thread(ctx, move |backend| {
        backend.add_tracks_to_sonos_playlist(&request)
    })
    .await
}
