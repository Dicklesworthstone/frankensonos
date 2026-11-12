use super::*;
use crate::{Cli, Command};
use clap::{CommandFactory, Parser};

const TRACK: &str = "spotify:track:4uLU6hMCjMI75M1A2tKUQC";
const EPISODE_LINK: &str = "https://open.spotify.com/episode/512ojhOuo1ktJprKbVcKyQ?si=test";

fn parsed(args: &[&str]) -> Result<Cli, clap::Error> {
    Cli::try_parse_from(
        ["fsonos", "queue-tracks"]
            .into_iter()
            .chain(args.iter().copied()),
    )
}

fn arguments(args: &[&str]) -> QueueTracksArgs {
    let Command::QueueTracks(args) = parsed(args).unwrap().command else {
        panic!("queue-tracks")
    };
    args
}

#[test]
fn selection_keeps_order_duplicates_and_links_for_shared_normalization() {
    let request = arguments(&["Member@S2", TRACK, EPISODE_LINK, TRACK])
        .request()
        .unwrap();
    assert_eq!(request.zone, "Member@S2");
    assert_eq!(
        request
            .tracks
            .iter()
            .map(|track| track.source_uri.as_str())
            .collect::<Vec<_>>(),
        [TRACK, EPISODE_LINK, TRACK]
    );
    assert!(request.tracks.iter().all(|track| track.title.is_none()));
    assert!(!request.next);
    assert!(request.expected_revision.is_none());
}

#[test]
fn next_and_destination_revision_are_forwarded_without_rewriting() {
    let request = arguments(&[
        "Room / ?@S2",
        TRACK,
        "--next",
        "--expected-revision",
        "q1:room&?#",
    ])
    .request()
    .unwrap();
    assert!(request.next);
    assert_eq!(request.expected_revision.as_deref(), Some("q1:room&?#"));
}

#[test]
fn invalid_selections_are_rejected_before_routing() {
    for args in [
        vec!["Room"],
        vec![" ", TRACK],
        vec!["Room", " "],
        vec!["Room", TRACK, "--expected-revision", " "],
    ] {
        assert!(parsed(&args).is_err(), "{args:?}");
    }
    let mut maximum = vec!["Room"];
    maximum.extend(std::iter::repeat_n(TRACK, MAX_QUEUE_TRACKS));
    assert_eq!(
        arguments(&maximum).request().unwrap().tracks.len(),
        MAX_QUEUE_TRACKS
    );
    maximum.push(TRACK);
    assert!(parsed(&maximum).is_err());
    for source in [
        "spotify:album:4uLU6hMCjMI75M1A2tKUQC",
        "spotify:playlist:4uLU6hMCjMI75M1A2tKUQC",
        "https://example.invalid/track/test",
    ] {
        assert!(arguments(&["Room", source]).request().is_err());
    }
    assert!(
        arguments(&["Room", TRACK, "--expected-revision", &"a".repeat(1025)])
            .request()
            .is_err()
    );
}

#[test]
fn json_daemon_direct_and_room_completion_are_available() {
    let cli = parsed(&[
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
    assert!(parsed(&["Room", TRACK, "--direct"]).unwrap().global.direct);
    assert!(parsed(&["Room", TRACK, "--daemon"]).unwrap().global.daemon);
    let command = Cli::command();
    command.clone().debug_assert();
    assert!(
        crate::completions::room_commands(&command)
            .iter()
            .any(|command| command == "queue-tracks")
    );
}
