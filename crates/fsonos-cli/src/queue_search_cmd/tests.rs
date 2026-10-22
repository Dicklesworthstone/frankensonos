use super::*;
use crate::{Cli, Command};
use clap::{CommandFactory, Parser};

fn parsed(args: &[&str]) -> Result<Cli, clap::Error> {
    Cli::try_parse_from(
        ["fsonos", "queue-search"]
            .into_iter()
            .chain(args.iter().copied()),
    )
}

fn arguments(args: &[&str]) -> QueueSearchArgs {
    let Command::QueueSearch(args) = parsed(args).unwrap().command else {
        panic!("queue-search")
    };
    args
}

#[test]
fn initial_search_uses_shared_defaults_and_keeps_literal_text() {
    let request = arguments(&["Member@S2", "Björk & strings"])
        .request()
        .unwrap();
    assert_eq!(request.zone, "Member@S2");
    assert_eq!(request.query, "Björk & strings");
    assert_eq!((request.offset, request.limit), (0, 50));
    assert!(request.expected_revision.is_none());
}

#[test]
fn continuation_keeps_exact_revision_offset_and_encoded_query() {
    let request = arguments(&[
        "Room / ?@S2",
        "Björk & strings",
        "--offset",
        "121",
        "--limit",
        "200",
        "--expected-revision",
        "q1:room&?#",
    ])
    .request()
    .unwrap();
    assert_eq!((request.offset, request.limit), (121, 200));
    assert_eq!(request.expected_revision.as_deref(), Some("q1:room&?#"));
    assert_eq!(
        request.path().unwrap(),
        "/zones/Room%20%2F%20%3F%40S2/queue/search?q=Bj%C3%B6rk%20%26%20strings&offset=121&limit=200&expected_revision=q1%3Aroom%26%3F%23"
    );
}

#[test]
fn malformed_bounds_and_blank_arguments_fail_before_routing() {
    for limit in ["0", "201", "-1", "1.5", "4294967296"] {
        assert!(parsed(&["Room", "song", "--limit", limit]).is_err());
    }
    for args in [
        vec!["Room"],
        vec![" ", "song"],
        vec!["Room", " "],
        vec!["Room", "song", "--expected-revision", " "],
    ] {
        assert!(parsed(&args).is_err());
    }
    for args in [vec!["Room", "song", "--offset", "1"], vec!["Room", "?!"]] {
        assert!(arguments(&args).request().is_err());
    }
    assert!(arguments(&["Room", &"a".repeat(1025)]).request().is_err());
}

#[test]
fn every_routing_mode_and_json_remain_available() {
    let cli = parsed(&[
        "Room",
        "song",
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
    assert!(parsed(&["Room", "song", "--direct"]).unwrap().global.direct);
    assert!(parsed(&["Room", "song", "--daemon"]).unwrap().global.daemon);
    let command = Cli::command();
    command.clone().debug_assert();
    assert!(
        crate::completions::room_commands(&command)
            .iter()
            .any(|c| c == "queue-search")
    );
}
