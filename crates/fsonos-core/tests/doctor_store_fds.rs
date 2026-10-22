//! The doctor's store check, in a process that holds the store (the daemon,
//! `fsonos mcp`), opens no second connection: fsqlite cannot close one while
//! the first holds its locks, so each check kept two descriptors open (seen
//! live: every GET /doctor on the owner's daemon added two, and the web
//! remote runs one on every load). A test binary of its own, so no other
//! test opens files while it counts them.

use fsonos_core::doctor::Status;
use fsonos_core::doctor::lan::store_check;
use fsonos_core::store::{FILE_NAME, SqliteStore};

/// This process's open descriptors.
#[cfg(unix)]
fn open_descriptors() -> usize {
    std::fs::read_dir("/dev/fd").expect("/dev/fd").count()
}

#[cfg(unix)]
#[test]
fn a_held_store_is_checked_without_another_descriptor() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(FILE_NAME);
    assert_eq!(SqliteStore::held(&path), None);
    let store = SqliteStore::open(&path).unwrap();
    let versions = SqliteStore::held(&path).expect("held while open");
    assert_eq!(versions.first(), Some(&1));

    let first = store_check(dir.path());
    assert_eq!(first.status, Status::Pass, "{first:?}");
    assert_eq!(first.evidence["held"], true);
    let before = open_descriptors();
    for _ in 0..10 {
        assert_eq!(store_check(dir.path()).status, Status::Pass);
    }
    assert_eq!(
        open_descriptors(),
        before,
        "each check kept descriptors open"
    );

    // Closed, the store is no longer held, and the check opens it itself.
    store.close().unwrap();
    assert_eq!(SqliteStore::held(&path), None);
    let alone = store_check(dir.path());
    assert_eq!(alone.status, Status::Pass, "{alone:?}");
    assert_eq!(alone.evidence.get("held"), None);
}
