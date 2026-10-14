use super::*;

fn request(position: u32, names: &[&str], before: u32) -> QueueMoveRequest {
    QueueMoveRequest {
        zone: "Room".into(),
        position,
        insert_before: before,
        expected_uris: names.iter().map(|s| (*s).into()).collect(),
        expected_revision: "observed-revision".into(),
    }
}

#[test]
fn destinations_use_original_positions_and_preserve_internal_order() {
    let queue: Vec<_> = ["a", "b", "c", "d", "e"].map(str::to_string).into();
    for (before, expected, position) in [
        (1, ["b", "c", "a", "d", "e"], 1),
        (5, ["a", "d", "b", "c", "e"], 3),
        (6, ["a", "d", "e", "b", "c"], 4),
    ] {
        let plan = request(2, &["b", "c"], before).plan(&queue).unwrap();
        assert_eq!(plan.uris, expected);
        assert_eq!(plan.position, position);
        assert!(plan.changes_order);
    }
    for before in 2..=4 {
        let plan = request(2, &["b", "c"], before).plan(&queue).unwrap();
        assert_eq!(plan.uris, queue);
        assert!(!plan.changes_order);
        assert_eq!(plan.position, 2);
    }
}

#[test]
fn invalid_ranges_and_stale_identities_are_not_clamped_to_other_entries() {
    let queue: Vec<_> = ["a", "b", "c"].map(str::to_string).into();
    for req in [
        request(0, &["a"], 2),
        request(1, &[], 2),
        request(1, &["a"], 0),
        request(1, &["a"], 5),
        request(3, &["c", "d"], 1),
        request(u32::MAX, &["c", "d"], 1),
        request(1, &["b"], 4),
        request(1, &[""], 4),
        request(1, &["NOT_IMPLEMENTED"], 4),
    ] {
        assert!(req.plan(&queue).is_err(), "{req:?}");
    }
}

#[test]
fn identical_uris_are_still_distinct_slots_not_an_automatic_noop() {
    let plan = request(1, &["a"], 3)
        .plan(&["a".into(), "a".into()])
        .unwrap();
    assert_eq!(plan.uris, ["a", "a"]);
    assert!(plan.changes_order);
    assert_eq!(plan.position, 2);
}

#[test]
fn a_move_cannot_decode_without_a_revision_or_silently_ignore_fields() {
    let mut json = serde_json::to_value(request(1, &["a"], 2)).unwrap();
    json.as_object_mut().unwrap().remove("expected_revision");
    assert!(serde_json::from_value::<QueueMoveRequest>(json).is_err());
    let mut json = serde_json::to_value(request(1, &["a"], 2)).unwrap();
    json["force"] = serde_json::json!(true);
    assert!(serde_json::from_value::<QueueMoveRequest>(json).is_err());
    let mut req = request(1, &["a"], 2);
    req.expected_revision = " ".into();
    assert!(req.validate().is_err());
}
