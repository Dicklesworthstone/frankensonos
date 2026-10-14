//! `fsonos queue-play <room> <position>`: activate an existing queue entry.
//! This command owns its daemon routing, like the DJ preference commands,
//! so an explicitly selected remote never falls back onto the local LAN.

use clap::Args;
use fsonos_api::OutcomeDto;
use fsonos_api::surface::queue::QueuePlayRequest;
use fsonos_core::policy::Client;

use crate::config::GlobalArgs;
use crate::remote::Daemon;
use crate::{daemon, emit, outcome_text};

#[derive(Debug, Args)]
pub struct QueuePlayArgs {
    /// A room, Room@S1 / Room@S2, here, or a single-room alias.
    #[arg(value_parser = nonblank)]
    pub zone: String,
    /// Existing position from `fsonos queue`, starting at one.
    #[arg(value_parser = clap::value_parser!(u32).range(1..))]
    pub position: u32,
    /// Exact renderer URI from `fsonos queue --json`; reject a changed item.
    #[arg(long, value_name = "URI", value_parser = nonblank)]
    pub expected_uri: Option<String>,
    /// Exact revision from `fsonos queue --json`; reject a changed queue.
    #[arg(long, value_name = "REVISION", value_parser = nonblank)]
    pub expected_revision: Option<String>,
}

fn nonblank(value: &str) -> Result<String, String> {
    if value.trim().is_empty() {
        Err("the value must not be blank".into())
    } else {
        // An expected URI is an exact identity check, not a normalized source.
        Ok(value.to_owned())
    }
}

impl QueuePlayArgs {
    #[must_use]
    pub fn request(&self) -> QueuePlayRequest {
        QueuePlayRequest {
            zone: self.zone.clone(),
            position: self.position,
            expected_uri: self.expected_uri.clone(),
            expected_revision: self.expected_revision.clone(),
        }
    }
}

pub fn run(global: &GlobalArgs, args: &QueuePlayArgs) -> anyhow::Result<()> {
    let request = args.request();
    let outcome: OutcomeDto = if let Some(daemon) = Daemon::find(global, global.daemon)? {
        daemon.post("/queue/play", &request)?
    } else {
        let dir = daemon::data_dir(global)?;
        let surface = daemon::surface(global, daemon::policy(&dir)?)?;
        let surface = daemon::with_action_log(surface, &dir, "cli");
        surface.queue_play(&Client::Cli, &request)?
    };
    emit(global.json, &outcome, outcome_text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cli, Command};
    use clap::Parser;

    const URI: &str = "x-sonos-spotify:spotify%3Atrack%3AQueueSelection?sid=12&sn=1";

    #[test]
    fn queue_play_preserves_selected_renderer_identity_and_global_remote_options() {
        let cli = Cli::try_parse_from([
            "fsonos",
            "queue-play",
            "Living Room@S2",
            "3",
            "--expected-uri",
            URI,
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
        let Command::QueuePlay(args) = cli.command else {
            panic!("expected an existing queue selection")
        };
        assert_eq!(
            args.request(),
            QueuePlayRequest {
                zone: "Living Room@S2".into(),
                position: 3,
                expected_uri: Some(URI.into()),
                expected_revision: None,
            }
        );
    }

    #[test]
    fn invalid_or_missing_queue_positions_fail_during_argument_parsing() {
        for position in ["0", "-1", "4294967296", "1.5", "first"] {
            assert!(Cli::try_parse_from(["fsonos", "queue-play", "Room", position]).is_err());
        }
        assert!(Cli::try_parse_from(["fsonos", "queue-play", "Room"]).is_err());
        assert!(Cli::try_parse_from(["fsonos", "queue-play", " ", "1"]).is_err());
        assert!(
            Cli::try_parse_from(["fsonos", "queue-play", "Room", "1", "--expected-uri", " ",])
                .is_err()
        );
    }

    #[test]
    fn listing_stays_read_only_and_direct_selection_needs_no_expected_uri() {
        let listed = Cli::try_parse_from(["fsonos", "queue", "Room", "--offset", "2"]).unwrap();
        assert!(matches!(listed.command, Command::Queue { offset: 2, .. }));
        let selected =
            Cli::try_parse_from(["fsonos", "queue-play", "Room", "1", "--direct"]).unwrap();
        assert!(selected.global.direct);
        let Command::QueuePlay(args) = selected.command else {
            panic!("expected queue selection")
        };
        assert_eq!(args.request().expected_uri, None);
        assert_eq!(args.request().position, 1);
    }

    #[test]
    fn queue_revision_is_forwarded_exactly_with_the_selected_uri() {
        let revision = "q1:9:test:room:4294967295:3";
        let cli = Cli::try_parse_from([
            "fsonos",
            "queue-play",
            "Room",
            "3",
            "--expected-uri",
            URI,
            "--expected-revision",
            revision,
            "--daemon",
        ])
        .unwrap();
        let Command::QueuePlay(args) = cli.command else {
            panic!("expected queue selection")
        };
        let body = serde_json::to_value(args.request()).unwrap();
        assert_eq!(body["expected_revision"], revision);
        assert_eq!(body["expected_uri"], URI);
        assert_eq!(body["position"], 3);
    }

    #[test]
    fn blank_revisions_are_rejected_and_omitted_revisions_stay_optional() {
        assert!(
            Cli::try_parse_from([
                "fsonos", "queue-play", "Room", "1", "--expected-revision", " ",
            ])
            .is_err()
        );
        let cli = Cli::try_parse_from(["fsonos", "queue-play", "Room", "1"]).unwrap();
        let Command::QueuePlay(args) = cli.command else {
            panic!("expected queue selection")
        };
        let body = serde_json::to_value(args.request()).unwrap();
        assert!(body.get("expected_revision").is_none());
    }
}
