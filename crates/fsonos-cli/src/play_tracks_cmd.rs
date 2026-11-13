//! Start an explicit ordered selection through the shared playback surface.

use clap::Args;
use fsonos_api::surface::queue::{MAX_QUEUE_TRACKS, PlayTracksRequest, QueueTrackInput};
use fsonos_api::{Failure, OutcomeDto};
use fsonos_core::policy::Client;

use crate::config::GlobalArgs;
use crate::remote::Daemon;
use crate::{daemon, emit, outcome_text};

#[derive(Debug, Args)]
pub struct PlayTracksArgs {
    /// A room, Room@S1 / Room@S2, here, or a single-room alias.
    #[arg(value_parser = nonblank)]
    zone: String,
    /// Spotify track or episode URIs or share links in the desired order.
    /// Supply 1-10; intentional duplicates stay. Albums/playlists are excluded.
    /// Replaces the queue and starts the first occurrence in normal order.
    #[arg(required = true, num_args = 1..=MAX_QUEUE_TRACKS, value_name = "URI", value_parser = nonblank)]
    source_uris: Vec<String>,
    /// Destination queue revision from `queue --json`. Refuse replacement if
    /// it changed; this is not a library or playlist revision.
    #[arg(long, value_name = "REVISION", value_parser = nonblank)]
    expected_revision: Option<String>,
}

fn nonblank(value: &str) -> Result<String, String> {
    if value.trim().is_empty() {
        Err("the value must not be blank".into())
    } else {
        Ok(value.to_owned())
    }
}

impl PlayTracksArgs {
    fn request(&self) -> Result<PlayTracksRequest, Failure> {
        let request = PlayTracksRequest {
            zone: self.zone.clone(),
            tracks: self
                .source_uris
                .iter()
                .map(|source_uri| QueueTrackInput {
                    source_uri: source_uri.clone(),
                    title: None,
                })
                .collect(),
            expected_revision: self.expected_revision.clone(),
        };
        request.validate()?;
        Ok(request)
    }
}

pub fn run(global: &GlobalArgs, args: &PlayTracksArgs) -> anyhow::Result<()> {
    let request = args.request()?;
    let outcome: OutcomeDto = if let Some(remote) = Daemon::find(global, global.daemon)? {
        // Once a daemon owns this replacement, even a lost reply is terminal.
        remote.post("/play/tracks", &request)?
    } else {
        let dir = daemon::data_dir(global)?;
        let surface = daemon::surface(global, daemon::policy(&dir)?)?;
        let surface = daemon::with_action_log(surface, &dir, "cli");
        surface.play_tracks(&Client::Cli, &request)?
    };
    emit(global.json, &outcome, outcome_text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cli, Command};
    use clap::{CommandFactory, Parser};

    const TRACK: &str = "spotify:track:1000000000000000000001";
    const EPISODE: &str = "https://open.spotify.com/episode/1000000000000000000002?si=fixture";

    fn args(values: &[&str]) -> Result<PlayTracksArgs, clap::Error> {
        let cli = Cli::try_parse_from(
            ["fsonos", "play-tracks"]
                .into_iter()
                .chain(values.iter().copied()),
        )?;
        let Command::PlayTracks(args) = cli.command else {
            panic!("play-tracks")
        };
        Ok(args)
    }

    #[test]
    fn explicit_order_repeats_and_destination_revision_survive_parsing() {
        let request = args(&[
            "Member@S2",
            TRACK,
            EPISODE,
            TRACK,
            "--expected-revision",
            "q1:original&?#",
        ])
        .unwrap()
        .request()
        .unwrap();
        assert_eq!(request.zone, "Member@S2");
        assert_eq!(
            request
                .tracks
                .iter()
                .map(|t| t.source_uri.as_str())
                .collect::<Vec<_>>(),
            [TRACK, EPISODE, TRACK]
        );
        assert_eq!(request.expected_revision.as_deref(), Some("q1:original&?#"));
        assert!(serde_json::to_value(request).unwrap().get("next").is_none());
    }

    #[test]
    fn invalid_or_oversized_selections_cannot_reach_backend_routing() {
        for values in [
            vec!["Room"],
            vec![" ", TRACK],
            vec!["Room", " "],
            vec!["Room", TRACK, "--next"],
            vec!["Room", TRACK, "--expected-revision", " "],
        ] {
            assert!(args(&values).is_err());
        }
        let mut maximum = vec!["Room"];
        maximum.extend(std::iter::repeat_n(TRACK, MAX_QUEUE_TRACKS));
        assert_eq!(
            args(&maximum).unwrap().request().unwrap().tracks.len(),
            MAX_QUEUE_TRACKS
        );
        maximum.push(TRACK);
        assert!(args(&maximum).is_err());
        for source in [
            "spotify:album:1000000000000000000001",
            "https://audio.example/stream",
            "spotify:track:short",
        ] {
            assert!(args(&["Room", source]).unwrap().request().is_err());
        }
    }

    #[test]
    fn global_modes_and_room_completion_include_ordered_playback() {
        let cli = Cli::try_parse_from([
            "fsonos",
            "play-tracks",
            "Room",
            TRACK,
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
        for flag in ["--direct", "--daemon"] {
            assert!(args(&["Room", TRACK, flag]).is_ok());
        }
        let command = Cli::command();
        command.clone().debug_assert();
        assert!(
            crate::completions::room_commands(&command)
                .iter()
                .any(|name| name == "play-tracks")
        );
    }
}
