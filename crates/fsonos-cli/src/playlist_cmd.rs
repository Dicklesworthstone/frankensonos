//! Inspect, select from and save native playlists through the selected daemon.

use clap::Args;
use fsonos_api::Failure;
use fsonos_api::surface::sonos_playlists::{
    SaveSonosPlaylistRequest, SavedSonosPlaylistDto, SonosPlaylistContentsDto,
    SonosPlaylistContentsRequest, SonosPlaylistTrackDto, SonosPlaylistTrackRequest,
};
use fsonos_core::playlists;
use fsonos_core::policy::Client;

use crate::config::GlobalArgs;
use crate::remote::Daemon;
use crate::{daemon, emit};

#[derive(Debug, Args)]
pub struct PlaylistContentsArgs {
    /// A room on the selected Sonos household (Room@S1 / Room@S2 disambiguates names).
    pub zone: String,
    /// Native SQ: id, current catalog number or unambiguous saved-playlist title.
    pub playlist: String,
    /// Continue at the preceding page's next_offset with its exact SQ: id and revision.
    #[arg(long, default_value_t = 0)]
    pub offset: u32,
    /// Maximum entries (1-100); reading never changes the playing queue.
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u32).range(1..=100))]
    pub limit: u32,
    /// Opaque revision from the preceding contents page; required for nonzero offsets.
    #[arg(long, value_name = "REVISION")]
    pub expected_revision: Option<String>,
}

impl PlaylistContentsArgs {
    fn request(&self) -> Result<SonosPlaylistContentsRequest, Failure> {
        let request = SonosPlaylistContentsRequest {
            zone: self.zone.clone(),
            playlist: self.playlist.clone(),
            offset: self.offset,
            limit: self.limit,
            expected_revision: self.expected_revision.clone(),
        };
        request.validate()?;
        Ok(request)
    }
}

pub fn contents(global: &GlobalArgs, args: &PlaylistContentsArgs) -> anyhow::Result<()> {
    let request = args.request()?;
    let path = request.path()?;
    let page: SonosPlaylistContentsDto = if let Some(remote) = Daemon::find(global, global.daemon)?
    {
        remote.get(&path)?
    } else {
        let dir = daemon::data_dir(global)?;
        let surface = daemon::surface(global, daemon::policy(&dir)?)?;
        surface.sonos_playlist_contents(&Client::Cli, &request)?
    };
    emit(global.json, &page, SonosPlaylistContentsDto::text)
}

#[derive(Debug, Args)]
pub struct PlaylistTrackArgs {
    /// A room on the playlist's Sonos household (Room@S1 / Room@S2 disambiguates names).
    pub zone: String,
    /// Exact native SQ: id from playlist-contents, not a title or catalog number.
    pub playlist: String,
    /// Original one-based track occurrence from playlist-contents.
    #[arg(value_parser = clap::value_parser!(u32).range(1..))]
    pub position: u32,
    /// Opaque revision from the page containing this occurrence.
    #[arg(long, value_name = "REVISION")]
    pub expected_revision: String,
    /// Play now while retaining the current queue, queue next, or append silently.
    #[arg(long, default_value = "play", value_parser = ["play", "next", "append"])]
    pub mode: String,
}

impl PlaylistTrackArgs {
    fn request(&self) -> Result<SonosPlaylistTrackRequest, Failure> {
        let request = SonosPlaylistTrackRequest {
            zone: self.zone.clone(),
            playlist: self.playlist.clone(),
            position: self.position,
            expected_revision: self.expected_revision.clone(),
            mode: self.mode.parse()?,
        };
        request.validate()?;
        Ok(request)
    }
}

pub fn track(global: &GlobalArgs, args: &PlaylistTrackArgs) -> anyhow::Result<()> {
    let request = args.request()?;
    let selected: SonosPlaylistTrackDto = if let Some(remote) = Daemon::find(global, global.daemon)?
    {
        remote.post("/sonos/playlists/track", &request)?
    } else {
        let dir = daemon::data_dir(global)?;
        let surface = daemon::surface(global, daemon::policy(&dir)?)?;
        let surface = daemon::with_action_log(surface, &dir, "cli");
        surface.sonos_playlist_track(&Client::Cli, &request)?
    };
    emit(global.json, &selected, SonosPlaylistTrackDto::text)
}

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
    fn selected_track_command_retains_exact_occurrence_mode_and_revision() {
        for mode in ["play", "next", "append"] {
            let cli = Cli::try_parse_from([
                "fsonos",
                "playlist-track",
                "Living Room@S2",
                "SQ:7",
                "3",
                "--expected-revision",
                "native:observed",
                "--mode",
                mode,
                "--daemon-url",
                "http://127.0.0.1:9999",
                "--json",
            ])
            .unwrap();
            assert!(cli.global.json);
            assert_eq!(
                cli.global.daemon_url.as_deref(),
                Some("http://127.0.0.1:9999")
            );
            let Command::PlaylistTrack(args) = cli.command else {
                panic!("expected selected track")
            };
            let request = args.request().unwrap();
            assert_eq!(request.zone, "Living Room@S2");
            assert_eq!(request.playlist, "SQ:7");
            assert_eq!(request.position, 3);
            assert_eq!(request.expected_revision, "native:observed");
            assert_eq!(request.mode, mode.parse().unwrap());
        }
        let cli = Cli::try_parse_from([
            "fsonos",
            "playlist-track",
            "Den@S1",
            "SQ:1",
            "1",
            "--expected-revision",
            "native:observed",
            "--direct",
        ])
        .unwrap();
        assert!(cli.global.direct);
        let Command::PlaylistTrack(args) = cli.command else {
            panic!("expected selected track")
        };
        assert_eq!(
            args.request().unwrap().mode,
            fsonos_api::surface::sonos_playlists::SonosPlaylistTrackMode::Play
        );
    }

    #[test]
    fn selected_track_command_refuses_missing_proofs_and_untrusted_sources() {
        assert!(Cli::try_parse_from(["fsonos", "playlist-track", "Den", "SQ:7", "3"]).is_err());
        for position in ["0", "-1", "4294967296"] {
            assert!(
                Cli::try_parse_from([
                    "fsonos",
                    "playlist-track",
                    "Den",
                    "SQ:7",
                    position,
                    "--expected-revision",
                    "r"
                ])
                .is_err()
            );
        }
        for option in ["--uri", "--metadata", "--source-uri", "--title"] {
            assert!(
                Cli::try_parse_from([
                    "fsonos",
                    "playlist-track",
                    "Den",
                    "SQ:7",
                    "3",
                    "--expected-revision",
                    "r",
                    option,
                    "untrusted"
                ])
                .is_err()
            );
        }
        for (playlist, revision, position) in [
            ("Evening", "r", "3"),
            ("SQ:7", " ", "3"),
            ("SQ:7", "r", "100001"),
        ] {
            let cli = Cli::try_parse_from([
                "fsonos",
                "playlist-track",
                "Den",
                playlist,
                position,
                "--expected-revision",
                revision,
            ])
            .unwrap();
            let Command::PlaylistTrack(args) = cli.command else {
                panic!("expected selected track")
            };
            assert!(args.request().is_err());
        }
    }

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

    #[test]
    fn native_contents_command_retains_daemon_target_and_revision() {
        let cli = Cli::try_parse_from([
            "fsonos",
            "playlist-contents",
            "Living Room@S2",
            "SQ:7",
            "--offset",
            "100",
            "--limit",
            "25",
            "--expected-revision",
            "sp1:observed",
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
        let Command::PlaylistContents(args) = cli.command else {
            panic!("expected playlist contents")
        };
        let request = args.request().unwrap();
        assert_eq!(request.zone, "Living Room@S2");
        assert_eq!(request.playlist, "SQ:7");
        assert_eq!(request.offset, 100);
        assert_eq!(request.limit, 25);
        assert_eq!(request.expected_revision.as_deref(), Some("sp1:observed"));
    }

    #[test]
    fn contents_rejects_unbound_continuations_before_daemon_discovery() {
        for arguments in [
            vec![
                "fsonos",
                "playlist-contents",
                "Den",
                "SQ:7",
                "--offset",
                "1",
            ],
            vec![
                "fsonos",
                "playlist-contents",
                "Den",
                "Evening",
                "--expected-revision",
                "revision",
            ],
            vec![
                "fsonos",
                "playlist-contents",
                "Den",
                "SQ:7",
                "--expected-revision",
                " ",
            ],
            vec!["fsonos", "playlist-contents", " ", "SQ:7"],
            vec!["fsonos", "playlist-contents", "Den", " "],
        ] {
            let cli = Cli::try_parse_from(arguments).unwrap();
            let Command::PlaylistContents(args) = cli.command else {
                panic!("expected playlist contents")
            };
            assert!(args.request().is_err());
        }
        for limit in ["0", "101", "4294967296"] {
            assert!(
                Cli::try_parse_from([
                    "fsonos",
                    "playlist-contents",
                    "Den",
                    "SQ:7",
                    "--limit",
                    limit
                ])
                .is_err()
            );
        }
        let cli = Cli::try_parse_from([
            "fsonos",
            "playlist-contents",
            "Den@S1",
            "Evening",
            "--direct",
        ])
        .unwrap();
        assert!(cli.global.direct);
        let Command::PlaylistContents(args) = cli.command else {
            panic!("expected playlist contents")
        };
        assert_eq!(args.request().unwrap().limit, 100);
    }
}
