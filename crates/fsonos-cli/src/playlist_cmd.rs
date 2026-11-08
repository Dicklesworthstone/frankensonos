//! Save a group's queue on Sonos, using the selected daemon when available.

use clap::Args;
use fsonos_api::Failure;
use fsonos_api::surface::sonos_playlists::{SaveSonosPlaylistRequest, SavedSonosPlaylistDto};
use fsonos_core::playlists;
use fsonos_core::policy::Client;

use crate::config::GlobalArgs;
use crate::remote::Daemon;
use crate::{daemon, emit};

#[derive(Debug, Args)]
pub struct PlaylistSaveArgs {
    /// A room, Room@S1 / Room@S2, here, or a single-room alias.
    pub zone: String,
    /// Title for the new playlist (at most 1023 UTF-8 bytes).
    #[arg(value_parser = title)]
    pub title: String,
}

fn title(value: &str) -> Result<String, String> {
    playlists::validate_title(value)
        .map(str::to_owned)
        .map_err(str::to_owned)
}

impl PlaylistSaveArgs {
    fn request(&self) -> Result<SaveSonosPlaylistRequest, Failure> {
        let request = SaveSonosPlaylistRequest {
            zone: self.zone.clone(),
            title: self.title.clone(),
        };
        request.validate()?;
        Ok(request)
    }
}

pub fn run(global: &GlobalArgs, args: &PlaylistSaveArgs) -> anyhow::Result<()> {
    // Reject invalid titles before daemon discovery or any speaker reads.
    let request = args.request()?;
    let saved: SavedSonosPlaylistDto = if let Some(daemon) = Daemon::find(global, global.daemon)? {
        daemon.post("/sonos/playlists/save", &request)?
    } else {
        let dir = daemon::data_dir(global)?;
        let surface = daemon::surface(global, daemon::policy(&dir)?)?;
        let surface = daemon::with_action_log(surface, &dir, "cli");
        surface.save_sonos_playlist(&Client::Cli, &request)?
    };
    emit(global.json, &saved, SavedSonosPlaylistDto::text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cli, Command};
    use clap::Parser;

    #[test]
    fn saving_preserves_the_household_title_and_selected_daemon() {
        let cli = Cli::try_parse_from([
            "fsonos",
            "playlist-save",
            "Living Room@S2",
            "  Dvořák & friends  ",
            "--json",
            "--daemon-url",
            "http://127.0.0.1:9999",
        ])
        .unwrap();
        assert!(cli.global.json);
        assert_eq!(
            cli.global.daemon_url.as_deref(),
            Some("http://127.0.0.1:9999")
        );
        let Command::PlaylistSave(args) = cli.command else {
            panic!("expected playlist save")
        };
        assert_eq!(
            args.request().unwrap(),
            SaveSonosPlaylistRequest {
                zone: "Living Room@S2".into(),
                title: "Dvořák & friends".into(),
            }
        );
    }

    #[test]
    fn saving_rejects_invalid_titles_and_all_overwrite_options() {
        for value in ["".into(), " \t ".into(), "A\nB".into(), "é".repeat(512)] {
            assert!(Cli::try_parse_from(["fsonos", "playlist-save", "Room", &value]).is_err());
        }
        for option in ["--overwrite", "--replace", "--playlist", "--object-id"] {
            assert!(
                Cli::try_parse_from(["fsonos", "playlist-save", "Room", "Mix", option, "SQ:1"])
                    .is_err()
            );
        }
        let cli = Cli::try_parse_from(["fsonos", "playlist-save", " ", "Mix"]).unwrap();
        let Command::PlaylistSave(args) = cli.command else {
            panic!("expected playlist save")
        };
        assert!(args.request().is_err());
        assert!(Cli::try_parse_from(["fsonos", "playlist-save", "Room"]).is_err());
    }

    #[test]
    fn listing_remains_separate_and_save_supports_explicit_direct_mode() {
        let listed = Cli::try_parse_from(["fsonos", "playlists", "Room"]).unwrap();
        assert!(matches!(listed.command, Command::Playlists { .. }));
        let saved =
            Cli::try_parse_from(["fsonos", "playlist-save", "Room", "Mix", "--direct"]).unwrap();
        assert!(saved.global.direct);
        assert!(matches!(saved.command, Command::PlaylistSave(_)));
    }
}
