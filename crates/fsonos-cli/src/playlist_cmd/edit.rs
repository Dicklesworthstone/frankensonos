//! Edit the saved playlist on the selected daemon, never its playing queue.

use clap::Args;
use fsonos_api::Failure;
use fsonos_api::surface::sonos_playlists::{
    EditedSonosPlaylistDto, MoveSonosPlaylistTracksRequest, RemoveSonosPlaylistTracksRequest,
};
use fsonos_core::playlists::MAX_PLAYLIST_EDIT_TRACKS;
use fsonos_core::policy::Client;

use crate::config::GlobalArgs;
use crate::remote::Daemon;
use crate::{daemon, emit};

#[derive(Debug, Args)]
pub struct PlaylistRemoveTracksArgs {
    /// A room in the playlist's household; Room@S1 / Room@S2 disambiguates names.
    pub zone: String,
    /// Exact native SQ: id from playlist-contents.
    pub playlist: String,
    /// First original one-based occurrence to remove from the SAVED playlist.
    #[arg(value_parser = clap::value_parser!(u32).range(1..))]
    pub position: u32,
    /// Consecutive occurrences to remove. The loaded playing queue is unchanged.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=i64::from(MAX_PLAYLIST_EDIT_TRACKS)))]
    pub count: u32,
    /// Opaque revision from a fresh playlist-contents read on this group.
    #[arg(long, value_name = "REVISION")]
    pub expected_revision: String,
}

impl PlaylistRemoveTracksArgs {
    fn request(&self) -> Result<RemoveSonosPlaylistTracksRequest, Failure> {
        let request = RemoveSonosPlaylistTracksRequest {
            zone: self.zone.clone(),
            playlist: self.playlist.clone(),
            expected_revision: self.expected_revision.clone(),
            position: self.position,
            count: self.count,
        };
        request.validate()?;
        Ok(request)
    }
}

#[derive(Debug, Args)]
pub struct PlaylistMoveTracksArgs {
    /// A room in the playlist's household; Room@S1 / Room@S2 disambiguates names.
    pub zone: String,
    /// Exact native SQ: id from playlist-contents.
    pub playlist: String,
    /// First original one-based occurrence of the saved block to move.
    #[arg(value_parser = clap::value_parser!(u32).range(1..))]
    pub position: u32,
    /// Consecutive entries; their internal order is retained.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=i64::from(MAX_PLAYLIST_EDIT_TRACKS)))]
    pub count: u32,
    /// Insert before this ORIGINAL one-based position; original total + 1 appends.
    #[arg(long = "before", value_parser = clap::value_parser!(u32).range(1..))]
    pub insert_before: u32,
    /// Opaque revision from a fresh playlist-contents read on this group.
    #[arg(long, value_name = "REVISION")]
    pub expected_revision: String,
}

impl PlaylistMoveTracksArgs {
    fn request(&self) -> Result<MoveSonosPlaylistTracksRequest, Failure> {
        let request = MoveSonosPlaylistTracksRequest {
            zone: self.zone.clone(),
            playlist: self.playlist.clone(),
            expected_revision: self.expected_revision.clone(),
            position: self.position,
            count: self.count,
            insert_before: self.insert_before,
        };
        request.validate()?;
        Ok(request)
    }
}

pub fn remove_tracks(global: &GlobalArgs, args: &PlaylistRemoveTracksArgs) -> anyhow::Result<()> {
    let request = args.request()?;
    let result: EditedSonosPlaylistDto = if let Some(remote) = Daemon::find(global, global.daemon)?
    {
        remote.post("/sonos/playlists/remove-tracks", &request)?
    } else {
        let dir = daemon::data_dir(global)?;
        let surface = daemon::surface(global, daemon::policy(&dir)?)?;
        daemon::with_action_log(surface, &dir, "cli")
            .remove_sonos_playlist_tracks(&Client::Cli, &request)?
    };
    emit(global.json, &result, EditedSonosPlaylistDto::text)
}

pub fn move_tracks(global: &GlobalArgs, args: &PlaylistMoveTracksArgs) -> anyhow::Result<()> {
    let request = args.request()?;
    let result: EditedSonosPlaylistDto = if let Some(remote) = Daemon::find(global, global.daemon)?
    {
        remote.post("/sonos/playlists/move-tracks", &request)?
    } else {
        let dir = daemon::data_dir(global)?;
        let surface = daemon::surface(global, daemon::policy(&dir)?)?;
        daemon::with_action_log(surface, &dir, "cli")
            .move_sonos_playlist_tracks(&Client::Cli, &request)?
    };
    emit(global.json, &result, EditedSonosPlaylistDto::text)
}

#[cfg(test)]
mod tests;
