use super::*;

fn queue(revision: u32, uris: &[&str]) -> Queue {
    Queue::new(revision, uris.iter().map(|uri| (*uri).to_owned()).collect())
}

fn slot(offset: u32, revision: u32, total: u32, uri: Option<&str>) -> Slot {
    Slot {
        offset,
        revision,
        total,
        uri: uri.map(str::to_owned),
    }
}

#[test]
fn append_and_middle_slots_are_bounded_before_any_write() {
    let before = queue(7, &["first", "last"]);
    assert_eq!(
        before.expected(0),
        Some(Expected {
            index: 2,
            position: 3,
            length: 3,
        })
    );
    assert_eq!(
        before.expected(2),
        Some(Expected {
            index: 1,
            position: 2,
            length: 3,
        })
    );
    assert_eq!(before.expected(3), before.expected(0));
    assert_eq!(before.expected(4), None);
    assert_eq!(before.expected(u32::MAX), None);
    assert_eq!(queue(1, &[]).expected(1), queue(1, &[]).expected(0));
}

#[test]
fn healthy_acknowledgement_must_identify_the_exact_occurrence() {
    let expected = queue(7, &["first", "last"]).expected(2).unwrap();
    assert!(expected.accepts((2, Some(3))));
    assert!(expected.accepts((2, None)));
    for ack in [
        (1, Some(3)),
        (3, Some(3)),
        (2, Some(2)),
        (2, Some(4)),
        (0, None),
    ] {
        assert!(!expected.accepts(ack));
    }
}

#[test]
fn a_single_revision_step_advances_the_complete_identity() {
    let mut before = queue(7, &["first", "last"]);
    let expected = before.expected(2).unwrap();
    let after = slot(1, 8, 3, Some("middle"));
    assert!(before.proves_single_step(&after, expected, "middle"));
    before.inserted(expected, "middle", after.revision);
    assert_eq!(before, queue(8, &["first", "middle", "last"]));
}

#[test]
fn duplicate_uris_still_require_the_requested_occurrence_and_count() {
    let before = queue(7, &["same", "last"]);
    let expected = before.expected(1).unwrap();
    assert!(before.proves_single_step(&slot(0, 8, 3, Some("same")), expected, "same"));
    assert!(!before.proves_single_step(&slot(1, 8, 3, Some("same")), expected, "same"));
    assert!(!before.proves_single_step(&slot(0, 8, 2, Some("same")), expected, "same"));
}

#[test]
fn unchanged_skipped_and_wrapped_counters_never_use_the_fast_path() {
    let before = queue(7, &["first"]);
    let expected = before.expected(0).unwrap();
    for revision in [0, 6, 7, 9, u32::MAX] {
        assert!(!before.proves_single_step(&slot(1, revision, 2, Some("new")), expected, "new"));
    }
    let before = queue(u32::MAX, &["first"]);
    let expected = before.expected(0).unwrap();
    assert!(!before.proves_single_step(&slot(1, 0, 2, Some("new")), expected, "new"));
    assert_eq!(
        observe(&before, &queue(0, &["first", "new"]), 1, "new"),
        Observed::Inserted
    );
}

#[test]
fn wrong_identity_wrong_count_and_missing_slot_require_full_recovery() {
    let before = queue(7, &["first"]);
    let expected = before.expected(0).unwrap();
    for observation in [
        slot(1, 8, 3, Some("new")),
        slot(1, 8, 1, Some("new")),
        slot(1, 8, 2, Some("other")),
        slot(1, 8, 2, None),
        slot(0, 8, 2, Some("new")),
        slot(u32::MAX, 8, 2, Some("new")),
    ] {
        assert!(!before.proves_single_step(&observation, expected, "new"));
    }
}

#[test]
fn head_checks_include_revision_length_first_identity_and_requested_offset() {
    let before = queue(7, &["first", "last"]);
    assert!(before.matches_head(&slot(0, 7, 2, Some("first"))));
    for observation in [
        slot(0, 8, 2, Some("first")),
        slot(0, 7, 3, Some("first")),
        slot(0, 7, 2, Some("other")),
        slot(0, 7, 2, None),
        slot(1, 7, 2, Some("first")),
    ] {
        assert!(!before.matches_head(&observation));
    }
    assert!(queue(7, &[]).matches_head(&slot(0, 7, 0, None)));
    assert!(!queue(7, &[]).matches_head(&slot(0, 7, 0, Some("ghost"))));
}

#[test]
fn same_count_prefix_edits_cannot_be_recovered_as_our_insertion() {
    let before = queue(7, &["a", "b", "a"]);
    assert_eq!(
        observe(&before, &queue(9, &["b", "a", "a", "new"]), 3, "new"),
        Observed::Indeterminate
    );
    assert_eq!(
        observe(&before, &queue(9, &["a", "b", "a", "new"]), 3, "new"),
        Observed::Inserted
    );
}

#[test]
fn retained_identity_bytes_are_bounded_across_successful_inserts() {
    let mut before = Queue::new(7, vec!["a".repeat(MAX_CACHED_URI_BYTES)]);
    assert!(before.cacheable());
    let expected = before.expected(0).unwrap();
    before.inserted(expected, "new", 8);
    assert!(!before.cacheable());
    assert!(!Queue::new(7, vec!["a".repeat(MAX_CACHED_URI_BYTES + 1)]).cacheable());
}
