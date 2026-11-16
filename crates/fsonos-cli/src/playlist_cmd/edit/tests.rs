use super::*;
use crate::{Cli, Command};
use clap::{CommandFactory, Parser};

#[test]
fn removal_and_moves_preserve_the_exact_reviewed_range_and_destination() {
    let cli = Cli::try_parse_from([
        "fsonos",
        "playlist-remove-tracks",
        "Den@S2",
        "SQ:7",
        "3",
        "--count",
        "2",
        "--expected-revision",
        "native:a&b=2",
        "--json",
        "--daemon",
    ])
    .unwrap();
    assert!(cli.global.json && cli.global.daemon);
    let Command::PlaylistRemoveTracks(args) = cli.command else {
        panic!("remove tracks")
    };
    assert_eq!(
        args.request().unwrap(),
        RemoveSonosPlaylistTracksRequest {
            zone: "Den@S2".into(),
            playlist: "SQ:7".into(),
            expected_revision: "native:a&b=2".into(),
            position: 3,
            count: 2,
        }
    );
    let cli = Cli::try_parse_from([
        "fsonos",
        "playlist-move-tracks",
        "Den@S1",
        "SQ:7",
        "2",
        "--before",
        "7",
        "--count",
        "3",
        "--expected-revision",
        "exact",
        "--direct",
    ])
    .unwrap();
    assert!(cli.global.direct);
    let Command::PlaylistMoveTracks(args) = cli.command else {
        panic!("move tracks")
    };
    assert_eq!(
        args.request().unwrap(),
        MoveSonosPlaylistTracksRequest {
            zone: "Den@S1".into(),
            playlist: "SQ:7".into(),
            expected_revision: "exact".into(),
            position: 2,
            count: 3,
            insert_before: 7,
        }
    );
}

#[test]
fn default_single_occurrence_still_requires_an_explicit_revision_and_move_destination() {
    let cli = Cli::try_parse_from([
        "fsonos",
        "playlist-remove-tracks",
        "Den",
        "SQ:7",
        "2",
        "--expected-revision",
        "r",
    ])
    .unwrap();
    let Command::PlaylistRemoveTracks(args) = cli.command else {
        panic!("remove tracks")
    };
    assert_eq!(args.request().unwrap().count, 1);
    assert!(
        Cli::try_parse_from(["fsonos", "playlist-remove-tracks", "Den", "SQ:7", "2",]).is_err()
    );
    assert!(
        Cli::try_parse_from([
            "fsonos",
            "playlist-move-tracks",
            "Den",
            "SQ:7",
            "2",
            "--expected-revision",
            "r",
        ])
        .is_err()
    );
}

#[test]
fn invalid_saved_ranges_or_identities_never_reach_daemon_discovery() {
    for (position, count) in [("0", "1"), ("1", "0"), ("1", "1001"), ("-1", "1")] {
        assert!(
            Cli::try_parse_from([
                "fsonos",
                "playlist-remove-tracks",
                "Den",
                "SQ:7",
                position,
                "--count",
                count,
                "--expected-revision",
                "r",
            ])
            .is_err()
        );
    }
    for (playlist, revision, position, count) in [
        ("Evening", "r", 1, 1),
        ("7", "r", 1, 1),
        ("SQ:7/1", "r", 1, 1),
        ("SQ:7", " ", 1, 1),
        ("SQ:7", "r", u32::MAX, 2),
    ] {
        let args = PlaylistRemoveTracksArgs {
            zone: "Den".into(),
            playlist: playlist.into(),
            expected_revision: revision.into(),
            position,
            count,
        };
        assert!(args.request().is_err());
    }
    Cli::command().debug_assert();
}
