use super::*;
use crate::{Cli, Command};
use clap::Parser;
use fsonos_api::surface::queue::QueueItemDto;
use serde_json::json;

fn page() -> QueueDto {
    QueueDto {
        zone: "Coordinator, not the requested room".into(),
        total: 8,
        revision: Some("q1:opaque observed revision".into()),
        playing: Some(3),
        items: (3..=6)
            .map(|position| QueueItemDto {
                position,
                title: format!("Entry {position}"),
                creator: None,
                album: None,
                uri: Some("renderer:duplicate?one=a,b&two=%27".into()),
            })
            .collect(),
    }
}

fn parse_remove(args: &[&str]) -> Result<Cli, clap::Error> {
    Cli::try_parse_from(
        ["fsonos", "queue-remove"]
            .into_iter()
            .chain(args.iter().copied()),
    )
}

fn remove(args: &[&str]) -> QueueRemoveArgs {
    let Command::QueueRemove(args) = parse_remove(args).unwrap().command else {
        panic!("queue-remove")
    };
    args
}

#[test]
fn removal_preserves_exact_uris_duplicates_and_revision() {
    let uri = " renderer:exact%27,a,b?x=1&y=2 ";
    let args = remove(&[
        "Room@S1",
        "3",
        "--expected-uri",
        uri,
        "--expected-uri",
        uri,
        "--expected-revision",
        " opaque ",
        "--daemon",
        "--json",
    ]);
    let request = args.request().unwrap();
    assert_eq!(request.zone, "Room@S1");
    assert_eq!(request.position, 3);
    assert_eq!(request.expected_uris, [uri, uri]);
    assert_eq!(request.expected_revision.as_deref(), Some(" opaque "));
}

#[test]
fn a_move_keeps_original_positions_and_global_remote_options() {
    let cli = Cli::try_parse_from([
        "fsonos",
        "queue-move",
        "Room@S2",
        "3",
        "9",
        "--expected-uri",
        "renderer:a",
        "--expected-uri",
        "renderer:b",
        "--expected-revision",
        "revision",
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
    let Command::QueueMove(args) = cli.command else {
        panic!("queue-move")
    };
    let request = args.request().unwrap();
    assert_eq!((request.position, request.insert_before), (3, 9));
    assert_eq!(request.expected_uris, ["renderer:a", "renderer:b"]);
    assert_eq!(request.expected_revision, "revision");
}

#[test]
fn missing_or_invalid_explicit_identity_never_parses() {
    for args in [
        vec!["Room", "1"],
        vec!["Room", "1", "--expected-uri", "renderer:a"],
        vec!["Room", "1", "--expected-revision", "r"],
        vec!["Room", "0", "--snapshot", "queue.json"],
        vec!["Room", "4294967296", "--snapshot", "queue.json"],
        vec![" ", "1", "--snapshot", "queue.json"],
    ] {
        assert!(parse_remove(&args).is_err());
    }
    for (uri, revision) in [("NOT_IMPLEMENTED", "r"), (" ", "r"), ("renderer:a", " ")] {
        assert!(
            parse_remove(&[
                "Room",
                "1",
                "--expected-uri",
                uri,
                "--expected-revision",
                revision,
            ])
            .is_err()
        );
    }
}

#[test]
fn snapshot_and_explicit_selection_cannot_be_mixed() {
    for extra in [
        vec!["--expected-uri", "renderer:a"],
        vec!["--expected-revision", "r"],
        vec!["--count", "0"],
        vec!["--count", "101"],
    ] {
        let mut args = vec!["Room", "1", "--snapshot", "queue.json"];
        args.extend(extra);
        assert!(parse_remove(&args).is_err());
    }
    assert!(
        parse_remove(&[
            "Room",
            "1",
            "--expected-uri",
            "renderer:a",
            "--expected-revision",
            "r",
            "--count",
            "2",
        ])
        .is_err()
    );
}

#[test]
fn explicit_ranges_are_bounded_before_any_daemon_lookup() {
    let mut args = remove(&["Room", "1", "--expected-uri", "u", "--expected-revision", "r"]);
    args.selection.expected_uris = vec!["u".into(); 100];
    assert!(args.request().is_ok());
    args.selection.expected_uris.push("u".into());
    assert!(args.request().is_err());
    args.selection.expected_uris.truncate(2);
    args.selection.position = u32::MAX;
    assert!(args.request().is_err());
}

#[test]
fn a_snapshot_selects_by_queue_position_not_page_index() {
    let p = page();
    let (uris, revision) = snapshot_selection(&p, 4, 2).unwrap();
    assert_eq!(
        uris,
        [
            p.items[1].uri.clone().unwrap(),
            p.items[2].uri.clone().unwrap()
        ]
    );
    assert_eq!(revision, p.revision.as_deref().unwrap());
    assert!(snapshot_selection(&p, 2, 2).is_err());
    assert!(snapshot_selection(&p, 6, 2).is_err());
    assert!(snapshot_selection(&p, 8, 2).is_err());
    assert!(snapshot_selection(&p, u32::MAX, 2).is_err());
}

#[test]
fn malformed_snapshot_order_or_revision_cannot_select_music() {
    for revision in [None, Some(String::new()), Some(" ".into())] {
        let mut p = page();
        p.revision = revision;
        assert!(snapshot_selection(&p, 3, 1).is_err());
    }
    for positions in [
        [3, 3, 5, 6],
        [3, 5, 6, 7],
        [6, 5, 4, 3],
        [0, 1, 2, 3],
        [6, 7, 8, 9],
    ] {
        let mut p = page();
        for (item, position) in p.items.iter_mut().zip(positions) {
            item.position = position;
        }
        assert!(snapshot_selection(&p, 3, 1).is_err());
    }
}

#[test]
fn missing_renderer_identities_are_not_reconstructed_from_titles() {
    for uri in [
        None,
        Some(String::new()),
        Some(" ".into()),
        Some("NOT_IMPLEMENTED".into()),
    ] {
        let mut p = page();
        p.items[0].uri = uri;
        assert!(snapshot_selection(&p, 3, 1).is_err());
        assert!(
            snapshot_selection(&p, 4, 1).is_ok(),
            "unselected identity need not be known"
        );
    }
}

#[test]
fn snapshot_input_is_bounded_and_must_be_a_complete_queue_document() {
    let bytes = serde_json::to_vec(&page()).unwrap();
    assert_eq!(read_snapshot(bytes.as_slice()).unwrap(), page());
    for value in [json!(null), json!({}), json!([]), json!({ "total": 2 })] {
        assert!(read_snapshot(value.to_string().as_bytes()).is_err());
    }
    assert!(read_snapshot(&bytes[..bytes.len() - 1]).is_err());
    let oversized = std::io::repeat(b' ').take(MAX_SNAPSHOT_BYTES + 2);
    assert!(read_snapshot(oversized).unwrap_err().detail.contains("1 MiB"));
}

#[test]
fn file_selection_uses_the_callers_destination_and_the_saved_revision() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queue.json");
    std::fs::write(&path, serde_json::to_vec(&page()).unwrap()).unwrap();
    let request = remove(&[
        "Member@S2",
        "4",
        "--snapshot",
        path.to_str().unwrap(),
        "--count",
        "2",
        "--direct",
    ])
    .request()
    .unwrap();
    assert_eq!(request.zone, "Member@S2");
    assert_eq!(request.position, 4);
    assert_eq!(request.expected_uris.len(), 2);
    assert_eq!(request.expected_revision, page().revision);
}

#[test]
fn move_snapshot_checks_destination_in_the_original_queue() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queue.json");
    std::fs::write(&path, serde_json::to_vec(&page()).unwrap()).unwrap();
    for (destination, valid) in [("1", true), ("9", true), ("10", false)] {
        let cli = Cli::try_parse_from([
            "fsonos",
            "queue-move",
            "Room",
            "3",
            destination,
            "--snapshot",
            path.to_str().unwrap(),
            "--count",
            "2",
        ])
        .unwrap();
        let Command::QueueMove(args) = cli.command else {
            panic!("queue-move")
        };
        assert_eq!(args.request().is_ok(), valid);
    }
}

#[test]
fn selection_count_defaults_to_one_and_stdin_is_an_explicit_snapshot() {
    let args = remove(&["Room", "3", "--snapshot", "-"]);
    assert_eq!(
        args.selection.snapshot.as_deref(),
        Some(std::path::Path::new("-"))
    );
    assert_eq!(args.selection.count, None);
    assert_eq!(snapshot_selection(&page(), 3, 1).unwrap().0.len(), 1);
}
