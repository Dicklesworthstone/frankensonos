//! Compose a saved playlist from reviewed native occurrences on its household.

use clap::Args;
use fsonos_api::Failure;
use fsonos_api::surface::sonos_playlists::{
    AppendSonosPlaylistTracksRequest, AppendedSonosPlaylistDto,
};
use fsonos_core::policy::Client;

use crate::config::GlobalArgs;
use crate::remote::Daemon;
use crate::{daemon, emit};

#[derive(Debug, Args)]
pub struct PlaylistAppendTracksArgs {
    /// A room in both playlists' household; Room@S1 / Room@S2 disambiguates names.
    pub zone: String,
    /// Exact native SQ: id of the destination saved playlist from playlist-contents.
    pub playlist: String,
    /// Destination revision from a fresh playlist-contents read on this group.
    #[arg(long, value_name = "REVISION")]
    pub expected_revision: String,
    /// Source revision from queue, or playlist-contents when --source-playlist is set.
    #[arg(long, value_name = "REVISION")]
    pub source_revision: String,
    /// One to ten one-based source occurrences in the desired order, e.g. 2,4,2.
    /// Repeated positions deliberately append the same occurrence again.
    #[arg(long, required = true, value_delimiter = ',', num_args = 1..=10, value_parser = clap::value_parser!(u32).range(1..))]
    pub positions: Vec<u32>,
    /// Exact source SQ: id in the same household. Omit to copy from this room's
    /// currently observed group queue; source playback and queue stay unchanged.
    #[arg(long, value_name = "SQ:ID")]
    pub source_playlist: Option<String>,
}

impl PlaylistAppendTracksArgs {
    fn request(&self) -> Result<AppendSonosPlaylistTracksRequest, Failure> {
        let request = AppendSonosPlaylistTracksRequest {
            zone: self.zone.clone(),
            playlist: self.playlist.clone(),
            expected_revision: self.expected_revision.clone(),
            source_playlist: self.source_playlist.clone(),
            source_revision: self.source_revision.clone(),
            positions: self.positions.clone(),
        };
        request.validate()?;
        Ok(request)
    }
}

pub fn append_tracks(global: &GlobalArgs, args: &PlaylistAppendTracksArgs) -> anyhow::Result<()> {
    let request = args.request()?;
    let result: AppendedSonosPlaylistDto =
        if let Some(remote) = Daemon::find(global, global.daemon)? {
            remote.post("/sonos/playlists/append-tracks", &request)?
        } else {
            let dir = daemon::data_dir(global)?;
            let surface = daemon::surface(global, daemon::policy(&dir)?)?;
            daemon::with_action_log(surface, &dir, "cli")
                .append_sonos_playlist_tracks(&Client::Cli, &request)?
        };
    emit(global.json, &result, AppendedSonosPlaylistDto::text)
}

#[cfg(test)]
mod tests;
