//! A store that fails for good, not for a busy moment, is noted where
//! `/health` and the doctor read it (vg3s.8). Its own binary: the fault
//! record is the process's, and other tests' stores must not see this one.

use fsonos_core::doctor::Status;
use fsonos_core::doctor::lan::store_check;
use fsonos_core::store::SqliteStore;
use fsonos_core::store::faults::{self, Kind};

#[test]
fn a_store_that_is_not_a_database_is_noted_as_a_storage_fault() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(faults::last(), None, "nothing failed yet");
    assert_eq!(store_check(dir.path()).status, Status::Pass);

    // Not a database: what a torn or overwritten file looks like.
    let bad = tempfile::tempdir().unwrap();
    let path = bad.path().join(fsonos_core::store::FILE_NAME);
    std::fs::write(&path, [0xAB_u8; 8192]).unwrap();
    let opened = SqliteStore::open(&path);
    assert!(opened.is_err(), "a garbage file opened as a store");
    let fault = faults::last().expect("the failed open is a storage fault");
    assert!(
        matches!(fault.kind, Kind::Corrupt | Kind::Io),
        "{fault:?}: {:?}",
        opened.err()
    );
    assert!(fault.count >= 1);

    // A store that opens again still fails the check while the fault is
    // recent: its writes have been failing, and actions went unlogged.
    let fresh = tempfile::tempdir().unwrap();
    let check = store_check(fresh.path());
    assert_eq!(check.status, Status::Fail, "{check:?}");
    assert!(check.summary.contains("have failed"), "{}", check.summary);
    assert_eq!(check.evidence["kind"], fault.kind.as_str());
}
