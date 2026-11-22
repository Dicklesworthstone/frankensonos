use super::*;
use crate::{Cli, Command};
use clap::{CommandFactory, Parser};

#[test]
fn append_preserves_both_revisions_order_and_repeats_from_either_source() {
    for source in [None, Some("SQ:3")] {
        let mut command = vec![
            "fsonos",
            "playlist-append-tracks",
            "Den@S2",
            "SQ:7",
            "--expected-revision",
            "destination:a&b=2",
            "--source-revision",
            "source:c&d=3",
            "--positions",
            "2,4,2",
            "--json",
            "--daemon-url",
            "http://127.0.0.1:9999",
        ];
        if let Some(source) = source {
            command.extend(["--source-playlist", source]);
        }
        let cli = Cli::try_parse_from(command).unwrap();
        assert!(cli.global.json);
        assert_eq!(
            cli.global.daemon_url.as_deref(),
            Some("http://127.0.0.1:9999")
        );
        let Command::PlaylistAppendTracks(args) = cli.command else {
            panic!("append saved tracks")
        };
        assert_eq!(
            args.request().unwrap(),
            AppendSonosPlaylistTracksRequest {
                zone: "Den@S2".into(),
                playlist: "SQ:7".into(),
                expected_revision: "destination:a&b=2".into(),
                source_playlist: source.map(str::to_owned),
                source_revision: "source:c&d=3".into(),
                positions: vec![2, 4, 2],
            }
        );
    }
}

#[test]
fn both_revisions_and_a_bounded_positive_selection_are_required() {
    let valid = [
        "fsonos",
        "playlist-append-tracks",
        "Den",
        "SQ:7",
        "--expected-revision",
        "destination",
        "--source-revision",
        "source",
        "--positions",
        "1,2",
    ];
    for option in ["--expected-revision", "--source-revision", "--positions"] {
        let index = valid.iter().position(|arg| *arg == option).unwrap();
        let mut incomplete = valid.to_vec();
        incomplete.drain(index..index + 2);
        assert!(Cli::try_parse_from(incomplete).is_err(), "{option}");
    }
    for positions in ["0", "1,0", "-1", "a", "1,,2"] {
        let mut invalid = valid.to_vec();
        invalid[9] = positions;
        assert!(Cli::try_parse_from(invalid).is_err(), "{positions}");
    }
    let mut oversized = valid.to_vec();
    oversized[9] = "1,2,3,4,5,6,7,8,9,10,11";
    if let Ok(cli) = Cli::try_parse_from(oversized) {
        let Command::PlaylistAppendTracks(args) = cli.command else {
            panic!("append saved tracks")
        };
        assert!(args.request().is_err());
    }
    Cli::command().debug_assert();
}

#[test]
fn invalid_native_identities_or_proofs_stop_before_daemon_discovery() {
    for (destination, revision, source, source_revision) in [
        ("Evening", "dest", None, "source"),
        ("7", "dest", None, "source"),
        ("SQ:7", " ", None, "source"),
        ("SQ:7", "dest", None, " "),
        ("SQ:7", "dest", Some("Morning"), "source"),
        ("SQ:7", "dest", Some("SQ:3/1"), "source"),
    ] {
        let args = PlaylistAppendTracksArgs {
            zone: "Den".into(),
            playlist: destination.into(),
            expected_revision: revision.into(),
            source_playlist: source.map(str::to_owned),
            source_revision: source_revision.into(),
            positions: vec![2, 1, 2],
        };
        assert!(args.request().is_err());
    }
}
