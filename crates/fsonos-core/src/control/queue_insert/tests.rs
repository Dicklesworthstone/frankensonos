use super::*;

fn queue(revision: u32, uris: &[&str]) -> Snapshot {
    Snapshot {
        revision,
        uris: uris.iter().map(|uri| (*uri).into()).collect(),
    }
}

#[test]
fn append_and_middle_insert_preserve_the_exact_neighbors() {
    let before = queue(4, &["a", "b"]);
    for (first, after) in [
        (1, queue(5, &["x", "a", "b"])),
        (2, queue(5, &["a", "x", "b"])),
        (3, queue(5, &["a", "b", "x"])),
    ] {
        assert_eq!(inserted(&before, &after, first, 1).unwrap(), ["x"]);
    }
}

#[test]
fn a_container_range_is_verified_without_inventing_its_expansion() {
    let before = queue(4, &["a", "b"]);
    let after = queue(5, &["a", "x", "y", "b"]);
    assert_eq!(inserted(&before, &after, 2, 2).unwrap(), ["x", "y"]);
    assert!(inserted(&before, &after, 2, 1).is_none());
}

#[test]
fn an_old_copy_never_proves_an_enqueue_happened() {
    let before = queue(4, &["x", "a", "x"]);
    assert!(inserted(&before, &before, 4, 1).is_none());
    assert!(inserted(&before, &queue(5, &["x", "a", "x"]), 4, 1).is_none());
    assert!(inserted(&before, &queue(5, &["x", "a", "x", "x"]), 4, 1).is_some());
}

#[test]
fn different_prefix_suffix_revision_or_count_is_not_confirmation() {
    let before = queue(4, &["a", "b"]);
    for after in [
        queue(5, &["z", "x", "b"]),
        queue(5, &["a", "x", "z"]),
        queue(4, &["a", "x", "b"]),
        queue(5, &["a", "x", "x", "b"]),
        queue(5, &["a", "b"]),
    ] {
        assert!(inserted(&before, &after, 2, 1).is_none());
    }
    let after = queue(5, &["a", "x", "b"]);
    for (first, count) in [(0, 1), (4, 1), (2, 0), (u32::MAX, u32::MAX)] {
        assert!(inserted(&before, &after, first, count).is_none());
    }
}

#[test]
fn empty_queues_and_revision_wrap_keep_the_same_identity_contract() {
    let before = queue(u32::MAX, &[]);
    let after = queue(0, &["x"]);
    assert_eq!(inserted(&before, &after, 1, 1).unwrap(), ["x"]);
}

#[test]
fn spotify_renderer_encodings_match_only_the_same_track() {
    assert!(same_track(
        "x-sonos-spotify:spotify%3atrack%3aTestA?sid=1",
        "spotify:track:TestA"
    ));
    assert!(same_track(
        "x-sonos-spotify:spotify%3Atrack%3ATestA?sid=2",
        "spotify:track:TestA"
    ));
    assert!(!same_track("spotify:track:TestA", "spotify:track:TestB"));
    assert!(!same_track(
        "http://example.invalid/a",
        "http://example.invalid/b"
    ));
}
