//! Add concrete Spotify sources to a saved playlist without editing the live queue.

use clap::Args;
use fsonos_api::Failure;
use fsonos_api::surface::queue::{MAX_QUEUE_TRACKS, QueueTrackInput};
use fsonos_api::surface::sonos_playlists::{
    AddTracksToSonosPlaylistRequest, AddedTracksToSonosPlaylistDto,
};
use fsonos_core::policy::Client;

use crate::config::GlobalArgs;
use crate::remote::Daemon;
use crate::{daemon, emit};

#[derive(Debug, Args)]
pub struct PlaylistAddTracksArgs {
    /// A room in the destination household; Room@S1 / Room@S2 disambiguates names.
    pub zone: String,
    /// Exact native SQ: id of the destination from playlist-contents.
    pub playlist: String,
    /// Spotify track or episode URIs or share links, in the desired order.
    /// Supply 1-10; repeated sources are retained. Containers are not expanded.
    #[arg(required = true, num_args = 1..=MAX_QUEUE_TRACKS, value_name = "URI")]
    pub source_uris: Vec<String>,
    /// Destination saved-playlist revision from a fresh playlist-contents read.
    /// This is not the live queue's revision or a cached-library revision.
    #[arg(long, value_name = "REVISION")]
    pub expected_revision: String,
}

impl PlaylistAddTracksArgs {
    fn request(&self) -> Result<AddTracksToSonosPlaylistRequest, Failure> {
        let request = AddTracksToSonosPlaylistRequest {
            zone: self.zone.clone(),
            playlist: self.playlist.clone(),
            expected_revision: self.expected_revision.clone(),
            tracks: self
                .source_uris
                .iter()
                .map(|source_uri| QueueTrackInput {
                    source_uri: source_uri.clone(),
                    title: None,
                })
                .collect(),
        };
        request.validate()?;
        Ok(request)
    }
}

pub fn add_tracks(global: &GlobalArgs, args: &PlaylistAddTracksArgs) -> anyhow::Result<()> {
    // Reject the entire selection before discovering a daemon or any speakers.
    let request = args.request()?;
    let result: AddedTracksToSonosPlaylistDto =
        if let Some(remote) = Daemon::find(global, global.daemon)? {
            // A failed selected-daemon reply never authorizes a local replay.
            remote.post("/sonos/playlists/add-tracks", &request)?
        } else {
            let dir = daemon::data_dir(global)?;
            let surface = daemon::surface(global, daemon::policy(&dir)?)?;
            daemon::with_action_log(surface, &dir, "cli")
                .add_tracks_to_sonos_playlist(&Client::Cli, &request)?
        };
    emit(global.json, &result, AddedTracksToSonosPlaylistDto::text)
}

#[cfg(test)]
mod tests;
