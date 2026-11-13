//! Prepared schedule actions need a durable claim of the exact row that was
//! checked, including its previous receipt. A process-local mutex cannot fence
//! another connection's pause, replacement or competing scheduler.

use fsonos_core::store::{MemStore, SqliteStore, Store, StoredSchedule};
use std::sync::{Arc, Barrier};

fn schedule(store: &mut dyn Store) -> StoredSchedule {
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

fn claims(store: &mut dyn Store) {
    let original = schedule(store);
    assert!(store.claim_schedule_fired(&original, 1_000).unwrap());
    assert!(!store.claim_schedule_fired(&original, 1_000).unwrap());
    assert!(!store.claim_schedule_fired(&original, 1_100).unwrap());
    let recorded = current(store, original.id);
    assert_eq!(recorded.last_fired, Some(1_000));
    assert!(!store.claim_schedule_fired(&recorded, 999).unwrap());
    assert!(!store.claim_schedule_fired(&recorded, 1_000).unwrap());
    assert!(store.claim_schedule_fired(&recorded, 1_100).unwrap());

    let observed = current(store, original.id);
    store.set_schedule_enabled(original.id, false).unwrap();
    assert!(!store.claim_schedule_fired(&observed, 1_200).unwrap());
    let paused = current(store, original.id);
    assert!(!store.claim_schedule_fired(&paused, 1_200).unwrap());
    assert_eq!(current(store, original.id), paused);

    // Resume bookkeeping still records missed runs while the row is disabled.
    assert!(store.mark_schedule_fired(original.id, 1_150).unwrap());
    store.set_schedule_enabled(original.id, true).unwrap();
    let resumed = current(store, original.id);
    assert!(!store.claim_schedule_fired(&observed, 1_200).unwrap());
    assert!(store.claim_schedule_fired(&resumed, 1_200).unwrap());

    let observed = current(store, original.id);
    for field in 0..5 {
        let mut replacement = observed.clone();
        match field {
            0 => replacement.spec = "daily 08:00".into(),
            1 => replacement.action = r#"{"kind":"pause","room":"Den"}"#.into(),
            2 => replacement.creator = "unknown".into(),
            3 => replacement.created += 1,
            _ => replacement.last_fired = Some(1_250),
        }
        store.delete_schedule(original.id).unwrap();
        assert!(store.put_schedule(&replacement).unwrap());
        assert!(!store.claim_schedule_fired(&observed, 1_300).unwrap());
        assert_eq!(current(store, original.id), replacement);
    }
    store.delete_schedule(original.id).unwrap();
    assert!(!store.claim_schedule_fired(&observed, 1_300).unwrap());
}

#[test]
fn memory_claims_require_the_current_enabled_definition_and_receipt() {
    claims(&mut MemStore::default());
}

#[test]
fn sqlite_claims_require_the_current_enabled_definition_and_receipt() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    claims(&mut store);
    store.close().unwrap();
}

#[test]
fn two_connections_cannot_claim_different_occurrences_from_the_same_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("schedule.db");
    let mut first = SqliteStore::open(&path).unwrap();
    let observed = schedule(&mut first);
    let mut second = SqliteStore::open(&path).unwrap();
    assert_eq!(current(&second, observed.id), observed);
    let ready = Arc::new(Barrier::new(2));
    let other_ready = Arc::clone(&ready);
    let other_observed = observed.clone();
    let first_claim = std::thread::spawn(move || {
        other_ready.wait();
        let claimed = first.claim_schedule_fired(&other_observed, 1_000).unwrap();
        first.close().unwrap();
        claimed
    });
    ready.wait();
    let second_claim = second.claim_schedule_fired(&observed, 1_100).unwrap();
    let first_claim = first_claim.join().unwrap();
    assert_ne!(first_claim, second_claim, "one observed row admits one run");
    let receipt = current(&second, observed.id).last_fired;
    assert_eq!(receipt, Some(if first_claim { 1_000 } else { 1_100 }));
    second.close().unwrap();
    let reopened = SqliteStore::open(&path).unwrap();
    assert_eq!(current(&reopened, observed.id).last_fired, receipt);
    reopened.close().unwrap();
}
