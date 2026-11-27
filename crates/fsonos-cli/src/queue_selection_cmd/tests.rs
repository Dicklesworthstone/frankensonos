use super::*;
use crate::{Cli, Command};
use clap::{CommandFactory, Parser};

fn parsed(args: &[&str]) -> Result<Cli, clap::Error> {
    Cli::try_parse_from(
        ["fsonos", "queue-selection"]
            .into_iter()
            .chain(args.iter().copied()),
    )
}

#[test]
fn source_occurrences_and_independent_destination_proofs_keep_their_identity() {
    let cli = parsed(&[
        "Member@S2",
        "3",
        "1",
        "3",
        "--target",
        "selected_s2",
        "--revision",
        "source-revision",
        "--target-revision",
        &"a".repeat(64),
        "--mode",
        "next",
        "--json",
        "--daemon",
    ])
    .unwrap();
    assert!(cli.global.json && cli.global.daemon);
    let Command::QueueSelection(args) = cli.command else {
        panic!("queue-selection")
    };
    let request = args.request().unwrap();
    assert_eq!(request.zone, "Member@S2");
    assert_eq!(request.positions, [3, 1, 3]);
    assert_eq!(request.target.as_deref(), Some("selected_s2"));
    assert_eq!(request.expected_revision, "source-revision");
    assert_eq!(request.expected_target_revision, Some("a".repeat(64)));
    assert!(request.expected_queue_revision.is_none());
}

#[test]
fn required_source_revision_mode_and_positions_cannot_be_omitted() {
    for args in [
        vec!["Den", "1", "--mode", "play"],
        vec!["Den", "1", "--revision", "source"],
        vec!["Den", "--revision", "source", "--mode", "play"],
        vec!["Den", "0", "--revision", "source", "--mode", "play"],
        vec!["Den", "1", "--revision", "source", "--mode", "unknown"],
    ] {
        assert!(parsed(&args).is_err(), "{args:?}");
    }
    let cli = parsed(&[
        "Den",
        "1",
        "--expected-revision",
        "source",
        "--mode",
        "append",
        "--expected-queue-revision",
        "destination",
        "--direct",
    ])
    .unwrap();
    let Command::QueueSelection(args) = cli.command else {
        panic!("queue-selection")
    };
    assert_eq!(
        args.request().unwrap().expected_queue_revision.as_deref(),
        Some("destination")
    );
    let command = Cli::command();
    command.clone().debug_assert();
    assert!(
        crate::completions::room_commands(&command)
            .iter()
            .any(|name| name == "queue-selection")
    );
}
