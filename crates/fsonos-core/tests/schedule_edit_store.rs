//! Schedule edits and removals must serialize with prepared-run admission,
//! including another daemon connection's occurrence receipt.

use fsonos_core::schedule::Schedule;
use fsonos_core::store::{MemStore, SqliteStore, Store, StoredSchedule};
use std::sync::{Arc, Barrier};

fn add(store: &mut dyn Store) -> StoredSchedule {
    let mut row = StoredSchedule {
        id: 0,
        spec: "daily 07:30".into(),
        action: r#"{"kind":"volume","room":"Den","level":20}"#.into(),
        creator: "cli".into(),
        enabled: true,
        created: 100,
        last_fired: None,
    };
    row.id = store.add_schedule(&row).unwrap();
    row
}

fn current(store: &dyn Store, id: i64) -> StoredSchedule {
    store
        .schedules()
        .unwrap()
        .into_iter()
        .find(|row| row.id == id)
        .unwrap()
}

fn replacement(observed: &StoredSchedule) -> StoredSchedule {
    StoredSchedule {
        spec: "weekdays 08:15".into(),
        action: r#"{"kind":"pause","room":"Office","fade_secs":30}"#.into(),
        last_fired: Some(900),
        ..observed.clone()
    }
}

fn edit_lifecycle(store: &mut dyn Store) {
    let original = add(store);
    let revised = replacement(&original);
    assert!(store.replace_schedule(&original, &revised).unwrap());
    assert_eq!(current(store, original.id), revised);
    assert!(!store.claim_schedule_fired(&original, 1_000).unwrap());
    assert!(
        !store.replace_schedule(&original, &original).unwrap(),
        "a stale no-op is not accepted"
    );
    assert!(!store.delete_schedule_if_unchanged(&original).unwrap());
    assert!(
        store.replace_schedule(&revised, &revised).unwrap(),
        "a current no-op is confirmed"
    );
    assert_eq!(current(store, original.id), revised);

    assert!(store.claim_schedule_fired(&revised, 1_000).unwrap());
    assert!(
        !store.replace_schedule(&revised, &revised).unwrap(),
        "a run changes the observed revision"
    );
    assert!(!store.delete_schedule_if_unchanged(&revised).unwrap());
    let observed = current(store, original.id);
    let paused = StoredSchedule {
        enabled: false,
        ..observed.clone()
    };
    assert!(store.replace_schedule(&observed, &paused).unwrap());
    assert!(!store.claim_schedule_fired(&observed, 1_100).unwrap());
    assert!(!store.claim_schedule_fired(&paused, 1_100).unwrap());
    let resumed = StoredSchedule {
        enabled: true,
        last_fired: Some(1_150),
        ..paused.clone()
    };
    assert!(store.replace_schedule(&paused, &resumed).unwrap());
    assert!(!store.claim_schedule_fired(&resumed, 1_100).unwrap());
    assert!(store.claim_schedule_fired(&resumed, 1_200).unwrap());
    assert!(!store.delete_schedule_if_unchanged(&resumed).unwrap());
    let observed = current(store, original.id);
    assert!(store.delete_schedule_if_unchanged(&observed).unwrap());
    assert!(!store.delete_schedule_if_unchanged(&observed).unwrap());
    assert!(!store.replace_schedule(&observed, &observed).unwrap());
    assert!(!store.claim_schedule_fired(&observed, 1_300).unwrap());
    assert!(store.schedules().unwrap().is_empty());
}

fn drift_refused(store: &mut dyn Store) {
    let mut observed = add(store);
    assert!(store.claim_schedule_fired(&observed, 1_000).unwrap());
    observed.last_fired = Some(1_000);
    let revised = StoredSchedule {
        spec: "daily 09:00".into(),
        ..observed.clone()
    };
    for field in 0..7 {
        let mut other = observed.clone();
        match field {
            0 => other.id += 10,
            1 => other.spec = "daily 08:30".into(),
            2 => other.action = r#"{"kind":"volume","room":"Den","level":25}"#.into(),
            3 => other.creator = "unknown".into(),
            4 => other.enabled = false,
            5 => other.created += 1,
            _ => other.last_fired = None,
        }
        assert!(store.delete_schedule(observed.id).unwrap());
        assert!(store.put_schedule(&other).unwrap());
        assert!(
            !store.replace_schedule(&observed, &revised).unwrap(),
            "field {field}"
        );
        assert!(
            !store.delete_schedule_if_unchanged(&observed).unwrap(),
            "field {field}"
        );
        assert_eq!(current(store, other.id), other);
        assert!(store.delete_schedule(other.id).unwrap());
        assert!(store.put_schedule(&observed).unwrap());
    }
    for field in 0..5 {
        let mut invalid = observed.clone();
        match field {
            0 => invalid.id += 1,
            1 => invalid.creator = "unknown".into(),
            2 => invalid.created += 1,
            3 => invalid.last_fired = None,
            _ => invalid.last_fired = Some(999),
        }
        assert!(
            store.replace_schedule(&observed, &invalid).is_err(),
            "field {field}"
        );
        assert_eq!(current(store, observed.id), observed);
    }
}

#[test]
fn memory_edits_keep_identity_and_receipts_and_fence_obsolete_operations() {
    edit_lifecycle(&mut MemStore::default());
    drift_refused(&mut MemStore::default());
}

#[test]
fn sqlite_edits_keep_identity_and_receipts_and_fence_obsolete_operations() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    edit_lifecycle(&mut store);
    drift_refused(&mut store);
    store.close().unwrap();
}

#[test]
fn two_connections_admit_either_the_reviewed_edit_or_the_old_prepared_run() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("schedule.db");
    let mut first = SqliteStore::open(&path).unwrap();
    let observed = add(&mut first);
    let revised = replacement(&observed);
    let mut second = SqliteStore::open(&path).unwrap();
    assert_eq!(current(&second, observed.id), observed);
    let ready = Arc::new(Barrier::new(2));
    let other_ready = Arc::clone(&ready);
    let other_observed = observed.clone();
    let other_revised = revised.clone();
    let editing = std::thread::spawn(move || {
        other_ready.wait();
        let edited = first
            .replace_schedule(&other_observed, &other_revised)
            .unwrap();
        first.close().unwrap();
        edited
    });
    ready.wait();
    let claimed = second.claim_schedule_fired(&observed, 1_000).unwrap();
    let edited = editing.join().unwrap();
    assert_ne!(
        edited, claimed,
        "an edit and the old run cannot both consume the same observation"
    );
    let expected = if edited {
        revised
    } else {
        StoredSchedule {
            last_fired: Some(1_000),
            ..observed.clone()
        }
    };
    assert_eq!(current(&second, observed.id), expected);
    assert!(!second.replace_schedule(&observed, &observed).unwrap());
    second.close().unwrap();
    let reopened = SqliteStore::open(&path).unwrap();
    assert_eq!(current(&reopened, observed.id), expected);
    reopened.close().unwrap();
}

#[test]
fn two_connections_cannot_both_replace_the_same_reviewed_definition() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("schedule.db");
    let mut first = SqliteStore::open(&path).unwrap();
    let observed = add(&mut first);
    let first_revision = replacement(&observed);
    let second_revision = StoredSchedule {
        spec: "weekends 09:00".into(),
        ..first_revision.clone()
    };
    let mut second = SqliteStore::open(&path).unwrap();
    let ready = Arc::new(Barrier::new(2));
    let other_ready = Arc::clone(&ready);
    let other_observed = observed.clone();
    let replacement = first_revision.clone();
    let editing = std::thread::spawn(move || {
        other_ready.wait();
        let edited = first
            .replace_schedule(&other_observed, &replacement)
            .unwrap();
        first.close().unwrap();
        edited
    });
    ready.wait();
    let second_edit = second
        .replace_schedule(&observed, &second_revision)
        .unwrap();
    let first_edit = editing.join().unwrap();
    assert_ne!(
        first_edit, second_edit,
        "only one reviewed definition can win"
    );
    let expected = if first_edit {
        first_revision
    } else {
        second_revision
    };
    assert_eq!(current(&second, observed.id), expected);
    assert!(!second.delete_schedule_if_unchanged(&observed).unwrap());
    second.close().unwrap();
    let reopened = SqliteStore::open(&path).unwrap();
    assert_eq!(current(&reopened, observed.id), expected);
    reopened.close().unwrap();
}

#[test]
fn removing_an_observed_definition_cannot_delete_a_concurrent_edit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("schedule.db");
    let mut editor = SqliteStore::open(&path).unwrap();
    let observed = add(&mut editor);
    let revised = replacement(&observed);
    let mut remover = SqliteStore::open(&path).unwrap();
    assert_eq!(current(&remover, observed.id), observed);
    assert!(editor.replace_schedule(&observed, &revised).unwrap());
    assert!(!remover.delete_schedule_if_unchanged(&observed).unwrap());
    assert_eq!(current(&remover, observed.id), revised);
    assert!(remover.delete_schedule_if_unchanged(&revised).unwrap());
    assert!(!editor.replace_schedule(&revised, &revised).unwrap());
    editor.close().unwrap();
    remover.close().unwrap();
}

#[test]
fn schedule_revisions_are_stable_and_bind_every_semantic_field() {
    let original = add(&mut MemStore::default());
    let revision = Schedule::from_stored(&original).unwrap().revision();
    assert_eq!(revision, "schedule-v1-9bfbf0bd1c9bd1fd");
    let mut encoded_differently = original.clone();
    encoded_differently.action =
        "{ \"level\": 20, \"room\": \"Den\", \"kind\": \"volume\" }".into();
    assert_eq!(
        Schedule::from_stored(&encoded_differently)
            .unwrap()
            .revision(),
        revision
    );
    for field in 0..7 {
        let mut changed = original.clone();
        match field {
            0 => changed.id += 1,
            1 => changed.spec = "daily 08:00".into(),
            2 => changed.action = r#"{"kind":"volume","room":"Den","level":25}"#.into(),
            3 => changed.creator = "unknown".into(),
            4 => changed.enabled = false,
            5 => changed.created += 1,
            _ => changed.last_fired = Some(1_000),
        }
        assert_ne!(
            Schedule::from_stored(&changed).unwrap().revision(),
            revision,
            "field {field}"
        );
    }
}
