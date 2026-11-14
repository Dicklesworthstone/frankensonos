//! Native catalog changes retain the exact identity and revision the user read.

use clap::Args;
use fsonos_api::Failure;
use fsonos_api::surface::sonos_playlists::{
    ManagedSonosPlaylistDto, RemoveSonosPlaylistRequest, RenameSonosPlaylistRequest,
};
use fsonos_core::policy::Client;

use crate::config::GlobalArgs;
use crate::remote::Daemon;
use crate::{daemon, emit};

#[derive(Debug, Args)]
pub struct PlaylistRenameArgs {
    /// A room in the playlist's household; Room@S1 / Room@S2 disambiguates names.
    pub zone: String,
    /// Exact native SQ: id from playlist-contents, never a title or catalog number.
    pub playlist: String,
    /// New title (nonblank, no control characters, at most 1023 UTF-8 bytes).
    #[arg(value_parser = super::title)]
    pub title: String,
    /// Opaque revision from a fresh playlist-contents read on this group.
    #[arg(long, value_name = "REVISION")]
    pub expected_revision: String,
}

impl PlaylistRenameArgs {
    fn request(&self) -> Result<RenameSonosPlaylistRequest, Failure> {
        let request = RenameSonosPlaylistRequest {
            zone: self.zone.clone(),
            playlist: self.playlist.clone(),
            expected_revision: self.expected_revision.clone(),
            title: self.title.clone(),
        };
        request.validate()?;
        Ok(request)
    }
}

#[derive(Debug, Args)]
pub struct PlaylistRemoveArgs {
    /// A room in the playlist's household; Room@S1 / Room@S2 disambiguates names.
    pub zone: String,
    /// Exact native SQ: id to permanently remove; playing queues are unchanged.
    pub playlist: String,
    /// Opaque revision from a fresh playlist-contents read on this group.
    #[arg(long, value_name = "REVISION")]
    pub expected_revision: String,
}

impl PlaylistRemoveArgs {
    fn request(&self) -> Result<RemoveSonosPlaylistRequest, Failure> {
        let request = RemoveSonosPlaylistRequest {
            zone: self.zone.clone(),
            playlist: self.playlist.clone(),
            expected_revision: self.expected_revision.clone(),
        };
        request.validate()?;
        Ok(request)
    }
}

pub fn rename(global: &GlobalArgs, args: &PlaylistRenameArgs) -> anyhow::Result<()> {
    // Validation must precede daemon discovery and all speaker reads.
    let request = args.request()?;
    let result: ManagedSonosPlaylistDto = if let Some(remote) = Daemon::find(global, global.daemon)?
    {
        remote.post("/sonos/playlists/rename", &request)?
    } else {
        let dir = daemon::data_dir(global)?;
        let surface = daemon::surface(global, daemon::policy(&dir)?)?;
        daemon::with_action_log(surface, &dir, "cli")
            .rename_sonos_playlist(&Client::Cli, &request)?
    };
    emit(global.json, &result, ManagedSonosPlaylistDto::text)
}

pub fn remove(global: &GlobalArgs, args: &PlaylistRemoveArgs) -> anyhow::Result<()> {
    let request = args.request()?;
    let result: ManagedSonosPlaylistDto = if let Some(remote) = Daemon::find(global, global.daemon)?
    {
        remote.post("/sonos/playlists/remove", &request)?
    } else {
        let dir = daemon::data_dir(global)?;
        let surface = daemon::surface(global, daemon::policy(&dir)?)?;
        daemon::with_action_log(surface, &dir, "cli")
            .remove_sonos_playlist(&Client::Cli, &request)?
    };
    emit(global.json, &result, ManagedSonosPlaylistDto::text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cli, Command};
    use clap::Parser;

    #[test]
    fn commands_preserve_household_native_identity_and_required_revision() {
        let cli = Cli::try_parse_from([
            "fsonos",
            "playlist-rename",
            "Den@S2",
            "SQ:7",
            "  Café, A & B  ",
            "--expected-revision",
            "native:a&b=2",
            "--json",
            "--daemon",
        ])
        .unwrap();
        assert!(cli.global.json);
        assert!(cli.global.daemon);
        let Command::PlaylistRename(args) = cli.command else {
            panic!("rename")
        };
        assert_eq!(
            args.request().unwrap(),
            RenameSonosPlaylistRequest {
                zone: "Den@S2".into(),
                playlist: "SQ:7".into(),
                expected_revision: "native:a&b=2".into(),
                title: "Café, A & B".into(),
            }
        );
        let cli = Cli::try_parse_from([
            "fsonos",
            "playlist-remove",
            "Den@S1",
            "SQ:7",
            "--direct",
            "--expected-revision",
            "observed",
        ])
        .unwrap();
        assert!(cli.global.direct);
        let Command::PlaylistRemove(args) = cli.command else {
            panic!("remove")
        };
        assert_eq!(
            args.request().unwrap(),
            RemoveSonosPlaylistRequest {
                zone: "Den@S1".into(),
                playlist: "SQ:7".into(),
                expected_revision: "observed".into(),
            }
        );
    }

    #[test]
    fn commands_refuse_missing_revision_invalid_title_and_ambiguous_identity() {
        assert!(Cli::try_parse_from(["fsonos", "playlist-remove", "Den", "SQ:7"]).is_err());
        assert!(Cli::try_parse_from(["fsonos", "playlist-rename", "Den", "SQ:7", "New"]).is_err());
        for title in [String::new(), "a\nb".into(), "é".repeat(512)] {
            assert!(
                Cli::try_parse_from([
                    "fsonos",
                    "playlist-rename",
                    "Den",
                    "SQ:7",
                    &title,
                    "--expected-revision",
                    "r",
                ])
                .is_err()
            );
        }
        for (playlist, revision) in [("Evening", "r"), ("7", "r"), ("SQ:7/1", "r"), ("SQ:7", " ")] {
            let rename = PlaylistRenameArgs {
                zone: "Den".into(),
                playlist: playlist.into(),
                title: "New".into(),
                expected_revision: revision.into(),
            };
            let remove = PlaylistRemoveArgs {
                zone: "Den".into(),
                playlist: playlist.into(),
                expected_revision: revision.into(),
            };
            assert!(rename.request().is_err());
            assert!(remove.request().is_err());
        }
    }
}
