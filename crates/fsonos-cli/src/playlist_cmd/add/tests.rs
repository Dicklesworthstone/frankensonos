use super::*;
use crate::{Cli, Command};
use clap::{CommandFactory, Parser};

const TRACK: &str = "spotify:track:0000000000000000000001";
const EPISODE: &str = "spotify:episode:0000000000000000000002";

#[test]
fn saved_addition_preserves_order_repeats_and_the_selected_daemon() {
    let cli = Cli::try_parse_from([
        "fsonos",
        "playlist-add-tracks",
        "Den@S2",
        "SQ:7",
        TRACK,
        EPISODE,
        TRACK,
        "--expected-revision",
        "saved:a&b=2",
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
    let Command::PlaylistAddTracks(args) = cli.command else {
        panic!("add explicit saved tracks")
    };
    let request = args.request().unwrap();
    assert_eq!(request.zone, "Den@S2");
    assert_eq!(request.playlist, "SQ:7");
    assert_eq!(request.expected_revision, "saved:a&b=2");
    assert_eq!(
        request
            .tracks
            .iter()
            .map(|track| track.source_uri.as_str())
            .collect::<Vec<_>>(),
        [TRACK, EPISODE, TRACK]
    );
    assert!(request.tracks.iter().all(|track| track.title.is_none()));
    Cli::command().debug_assert();
}

#[test]
fn saved_addition_requires_a_revision_and_a_bounded_source_selection() {
    assert!(Cli::try_parse_from(["fsonos", "playlist-add-tracks", "Den", "SQ:7", TRACK]).is_err());
    assert!(
        Cli::try_parse_from([
            "fsonos",
            "playlist-add-tracks",
            "Den",
            "SQ:7",
            "--expected-revision",
            "saved",
        ])
        .is_err()
    );
    let mut oversized = vec!["fsonos", "playlist-add-tracks", "Den", "SQ:7"];
    oversized.extend([TRACK; 11]);
    oversized.extend(["--expected-revision", "saved"]);
    if let Ok(cli) = Cli::try_parse_from(oversized) {
        let Command::PlaylistAddTracks(args) = cli.command else {
            panic!("add explicit saved tracks")
        };
        assert!(args.request().is_err());
    }
}

#[test]
fn invalid_sources_and_saved_proofs_stop_before_daemon_discovery() {
    for (playlist, revision, source) in [
        ("Evening", "saved", TRACK),
        ("SQ:7/1", "saved", TRACK),
        ("SQ:7", " ", TRACK),
        ("SQ:7", "saved\nother", TRACK),
        ("SQ:7", "saved", "spotify:album:0000000000000000000001"),
        ("SQ:7", "saved", "https://example.invalid/music.mp3"),
        ("SQ:7", "saved", "spotify:track:bad"),
    ] {
        let args = PlaylistAddTracksArgs {
            zone: "Den".into(),
            playlist: playlist.into(),
            expected_revision: revision.into(),
            source_uris: vec![source.into()],
        };
        assert!(args.request().is_err(), "{playlist} / {source}");
    }
}
