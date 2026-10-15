use super::*;
use crate::dj_cmd::DjAction;
use clap::Parser;

#[derive(Parser)]
struct Parse {
    #[command(subcommand)]
    action: DjAction,
}

fn parse(args: &[&str]) -> DjAction {
    Parse::try_parse_from(std::iter::once("dj").chain(args.iter().copied()))
        .unwrap()
        .action
}

#[test]
fn browsing_is_a_read_and_never_a_planned_speaker_control() {
    let action = parse(&["browse"]);
    assert!(action.is_read());
    assert_eq!(action.tool(), "browse_library");
    assert!(action.daemon_request().is_none());
    let DjAction::Browse(args) = action else {
        panic!("expected cached browsing");
    };
    assert_eq!(
        args.query().path().unwrap(),
        "/library/browse?kind=tracks&offset=0&limit=50"
    );
}

#[test]
fn paging_album_and_literal_filters_keep_exact_identities() {
    let DjAction::Browse(args) = parse(&[
        "browse",
        "tracks",
        "--album-uri",
        "spotify:album:TestAlbum",
        "--q",
        "Björk & Guests",
        "--offset",
        "20",
        "--limit",
        "20",
        "--expected-revision",
        "cache-before",
    ]) else {
        panic!("expected cached browsing");
    };
    assert_eq!(args.expected_revision.as_deref(), Some("cache-before"));
    assert_eq!(
        args.query().path().unwrap(),
        "/library/browse?kind=tracks&offset=20&limit=20&q=Bj%C3%B6rk%20%26%20Guests&album_uri=spotify%3Aalbum%3ATestAlbum"
    );
}

#[test]
fn unsupported_categories_and_page_sizes_are_not_silently_clamped() {
    for args in [
        vec!["dj", "browse", "artists"],
        vec!["dj", "browse", "--limit", "0"],
        vec!["dj", "browse", "--limit", "51"],
        vec!["dj", "browse", "--offset", "-1"],
        vec!["dj", "browse", "--offset", "4294967296"],
    ] {
        assert!(Parse::try_parse_from(args).is_err());
    }
    for args in [
        vec!["browse", "albums", "--album-uri", "spotify:album:A"],
        vec!["browse", "tracks", "--album-uri", "spotify:track:A"],
    ] {
        let DjAction::Browse(args) = parse(&args) else {
            panic!("expected cached browsing");
        };
        assert!(args.query().path().is_err());
    }
}
