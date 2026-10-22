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

#[test]
fn playlist_browsing_forwards_occurrence_paging_and_revision() {
    let action = parse(&[
        "browse",
        "tracks",
        "--playlist-uri",
        "spotify:playlist:ObservedList",
        "--q",
        "Björk & Guests",
        "--offset",
        "2",
        "--limit",
        "3",
        "--expected-revision",
        "playlist-observation",
    ]);
    assert!(action.is_read());
    assert_eq!(action.tool(), "browse_library");
    assert!(action.daemon_request().is_none());
    let DjAction::Browse(args) = action else {
        panic!("expected cached browsing");
    };
    let query = args.playlist_query().unwrap().unwrap();
    assert_eq!(query.playlist_uri, "spotify:playlist:ObservedList");
    assert_eq!(query.q.as_deref(), Some("Björk & Guests"));
    assert_eq!((query.offset, query.limit), (Some(2), Some(3)));
    assert_eq!(
        query.expected_revision.as_deref(),
        Some("playlist-observation")
    );
    let path = query.path().unwrap();
    assert!(path.starts_with("/library/playlist?"), "{path}");
    for field in [
        "playlist_uri=spotify%3Aplaylist%3AObservedList",
        "q=Bj%C3%B6rk%20%26%20Guests",
        "offset=2",
        "limit=3",
        "expected_revision=playlist-observation",
    ] {
        assert!(path.contains(field), "missing {field}: {path}");
    }
}

#[test]
fn playlist_filters_reject_mixed_sources_and_unpinned_continuation() {
    assert!(
        Parse::try_parse_from([
            "dj",
            "browse",
            "tracks",
            "--album-uri",
            "spotify:album:TestAlbum",
            "--playlist-uri",
            "spotify:playlist:ObservedList",
        ])
        .is_err()
    );
    for args in [
        vec![
            "browse",
            "albums",
            "--playlist-uri",
            "spotify:playlist:List",
        ],
        vec![
            "browse",
            "playlists",
            "--playlist-uri",
            "spotify:playlist:List",
        ],
        vec!["browse", "tracks", "--playlist-uri", "spotify:track:Wrong"],
        vec![
            "browse",
            "tracks",
            "--playlist-uri",
            "spotify:playlist:List",
            "--offset",
            "1",
        ],
    ] {
        let DjAction::Browse(args) = parse(&args) else {
            panic!("expected cached browsing");
        };
        assert!(args.playlist_query().is_err());
    }
    let DjAction::Browse(args) =
        parse(&["browse", "--playlist-uri", "spotify:playlist:ObservedList"])
    else {
        panic!("expected cached browsing");
    };
    let first = args.playlist_query().unwrap().unwrap();
    assert_eq!((first.offset, first.limit), (Some(0), Some(50)));
    assert!(first.expected_revision.is_none());
    let DjAction::Browse(args) = parse(&["browse"]) else {
        panic!("expected cached browsing");
    };
    assert!(args.playlist_query().unwrap().is_none());
}
