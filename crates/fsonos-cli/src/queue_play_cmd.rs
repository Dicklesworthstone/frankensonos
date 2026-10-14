//! `fsonos queue-play <room> <position>`: activate an existing queue entry.
//! This command owns its daemon routing, like the DJ preference commands,
//! so an explicitly selected remote never falls back onto the local LAN.

use clap::Args;
use fsonos_api::surface::queue::QueuePlayRequest;
use fsonos_api::{Failure, OutcomeDto};
use fsonos_core::policy::Client;
use std::path::PathBuf;

use crate::config::GlobalArgs;
use crate::queue_edit_cmd::{load_snapshot, snapshot_selection};
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
    /// A reviewed queue --json page containing this position and its revision.
    /// At most 1 MiB; - reads stdin. Cannot be mixed with explicit guards.
    #[arg(long, value_name = "FILE", conflicts_with_all = ["expected_uri", "expected_revision"])]
    pub snapshot: Option<PathBuf>,
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
    pub fn request(&self) -> Result<QueuePlayRequest, Failure> {
        let (expected_uri, expected_revision) = if let Some(path) = &self.snapshot {
            let page = load_snapshot(path)?;
            let (mut uris, revision) = snapshot_selection(&page, self.position, 1)?;
            let uri = uris
                .pop()
                .ok_or_else(|| Failure::invalid("the snapshot has no selected queue entry"))?;
            (Some(uri), Some(revision))
        } else {
            (self.expected_uri.clone(), self.expected_revision.clone())
        };
        Ok(QueuePlayRequest {
            zone: self.zone.clone(),
            position: self.position,
            expected_uri,
            expected_revision,
        })
    }
}

pub fn run(global: &GlobalArgs, args: &QueuePlayArgs) -> anyhow::Result<()> {
    // Validate a saved observation before even probing a daemon or the LAN.
    let request = args.request()?;
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
            args.request().unwrap(),
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
        assert_eq!(args.request().unwrap().expected_uri, None);
        assert_eq!(args.request().unwrap().position, 1);
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
        let body = serde_json::to_value(args.request().unwrap()).unwrap();
        assert_eq!(body["expected_revision"], revision);
        assert_eq!(body["expected_uri"], URI);
        assert_eq!(body["position"], 3);
    }

    #[test]
    fn blank_revisions_are_rejected_and_omitted_revisions_stay_optional() {
        assert!(
            Cli::try_parse_from([
                "fsonos",
                "queue-play",
                "Room",
                "1",
                "--expected-revision",
                " ",
            ])
            .is_err()
        );
        let cli = Cli::try_parse_from(["fsonos", "queue-play", "Room", "1"]).unwrap();
        let Command::QueuePlay(args) = cli.command else {
            panic!("expected queue selection")
        };
        let body = serde_json::to_value(args.request().unwrap()).unwrap();
        assert!(body.get("expected_revision").is_none());
    }

    fn from_snapshot(path: &str, position: &str) -> QueuePlayArgs {
        let cli = Cli::try_parse_from([
            "fsonos",
            "queue-play",
            "Member@S2",
            position,
            "--snapshot",
            path,
        ])
        .unwrap();
        let Command::QueuePlay(args) = cli.command else {
            panic!("queue-play")
        };
        args
    }

    fn page() -> serde_json::Value {
        serde_json::json!({
            "zone": "Display coordinator",
            "total": 20,
            "playing": 12,
            "revision": "exact revision",
            "items": [{ "position": 12, "title": "Track", "uri": URI }]
        })
    }

    #[test]
    fn snapshot_playback_binds_both_guards_and_keeps_the_callers_room() {
        let dir = crate::queue_edit_cmd::fixture::Directory::new();
        let path = dir.path().join("queue.json");
        std::fs::write(&path, page().to_string()).unwrap();
        let request = from_snapshot(path.to_str().unwrap(), "12")
            .request()
            .unwrap();
        assert_eq!(request.zone, "Member@S2");
        assert_eq!(request.position, 12);
        assert_eq!(request.expected_uri.as_deref(), Some(URI));
        assert_eq!(request.expected_revision.as_deref(), Some("exact revision"));
    }

    #[test]
    fn absent_positions_or_identities_do_not_become_unprotected_playback() {
        let dir = crate::queue_edit_cmd::fixture::Directory::new();
        let path = dir.path().join("queue.json");
        std::fs::write(&path, page().to_string()).unwrap();
        assert!(
            from_snapshot(path.to_str().unwrap(), "11")
                .request()
                .is_err()
        );
        for field in ["uri", "revision"] {
            let mut invalid = page();
            if field == "uri" {
                invalid["items"][0][field] = serde_json::Value::Null;
            } else {
                invalid[field] = serde_json::Value::Null;
            }
            std::fs::write(&path, invalid.to_string()).unwrap();
            assert!(
                from_snapshot(path.to_str().unwrap(), "12")
                    .request()
                    .is_err()
            );
        }
    }

    #[test]
    fn unreadable_or_oversized_snapshot_input_fails_before_backend_routing() {
        let dir = crate::queue_edit_cmd::fixture::Directory::new();
        let path = dir.path().join("queue.json");
        let args = from_snapshot(path.to_str().unwrap(), "12");
        assert!(args.request().is_err());
        std::fs::write(&path, "incomplete").unwrap();
        assert!(args.request().is_err());
        std::fs::write(&path, vec![b' '; 1024 * 1024 + 1]).unwrap();
        assert!(args.request().unwrap_err().detail.contains("1 MiB"));
    }

    #[test]
    fn snapshots_conflict_with_explicit_guards_and_accept_stdin() {
        for flag in ["--expected-uri", "--expected-revision"] {
            assert!(
                Cli::try_parse_from([
                    "fsonos",
                    "queue-play",
                    "Room",
                    "1",
                    "--snapshot",
                    "queue.json",
                    flag,
                    "identity",
                ])
                .is_err()
            );
        }
        assert_eq!(from_snapshot("-", "12").snapshot, Some(PathBuf::from("-")));
    }
}
