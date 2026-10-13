//! Lossless, retryable movement of stored DJ steering. Store implementations
//! need not expose a transaction: write the destination before deleting the
//! source, so a failed write or cleanup never discards the only copy.

use crate::store::{DjSession, Store, StoreError};
use fsonos_types::PlayerId;

/// The session-only seam lets failure tests exercise the same operation
/// ordering as the public Store adapter, without replacing unrelated storage.
trait Sessions {
    fn get(&self, coordinator: &str) -> Result<Option<DjSession>, StoreError>;
    fn save(&mut self, session: &DjSession) -> Result<(), StoreError>;
    fn delete(&mut self, coordinator: &str) -> Result<(), StoreError>;
}

struct Stored<'a>(&'a mut dyn Store);

impl Sessions for Stored<'_> {
    fn get(&self, coordinator: &str) -> Result<Option<DjSession>, StoreError> {
        self.0.dj_session(coordinator)
    }

    fn save(&mut self, session: &DjSession) -> Result<(), StoreError> {
        self.0.save_dj_session(session)
    }

    fn delete(&mut self, coordinator: &str) -> Result<(), StoreError> {
        self.0.delete_dj_session(coordinator)
    }
}

pub(super) fn rekey(
    store: &mut dyn Store,
    old: &PlayerId,
    new: &PlayerId,
) -> Result<bool, StoreError> {
    transfer(&mut Stored(store), &old.0, &new.0)
}

fn transfer(rows: &mut impl Sessions, old: &str, new: &str) -> Result<bool, StoreError> {
    let Some(mut session) = rows.get(old)? else {
        return Ok(false);
    };
    if old == new {
        return Ok(true);
    }
    session.coordinator = new.to_string();
    rows.save(&session)?;
    // A failed deletion leaves both copies. Retrying writes the same
    // destination and retries cleanup; it never needs to reconstruct state.
    rows.delete(old)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct Rows {
        sessions: BTreeMap<String, DjSession>,
        fail: Option<&'static str>,
        calls: RefCell<Vec<&'static str>>,
    }

    impl Rows {
        fn call(&self, action: &'static str) -> Result<(), StoreError> {
            self.calls.borrow_mut().push(action);
            if self.fail == Some(action) {
                return Err(StoreError::Backend(format!("injected {action} failure")));
            }
            Ok(())
        }
    }

    impl Sessions for Rows {
        fn get(&self, coordinator: &str) -> Result<Option<DjSession>, StoreError> {
            self.call("read")?;
            Ok(self.sessions.get(coordinator).cloned())
        }

        fn save(&mut self, session: &DjSession) -> Result<(), StoreError> {
            self.call("save")?;
            self.sessions
                .insert(session.coordinator.clone(), session.clone());
            Ok(())
        }

        fn delete(&mut self, coordinator: &str) -> Result<(), StoreError> {
            self.call("delete")?;
            self.sessions.remove(coordinator);
            Ok(())
        }
    }

    fn session(coordinator: &str, mood: &str) -> DjSession {
        DjSession {
            coordinator: coordinator.into(),
            mood: Some(mood.into()),
            constraints: Some("{\"exclude_keywords\":[\"vocal\"]}".into()),
            expires: 123_456,
        }
    }

    fn rows() -> Rows {
        Rows {
            sessions: BTreeMap::from([
                ("old".into(), session("old", "calm")),
                ("new".into(), session("new", "bright")),
            ]),
            ..Rows::default()
        }
    }

    #[test]
    fn failed_destination_write_preserves_both_original_sessions() {
        let mut rows = rows();
        let before = rows.sessions.clone();
        rows.fail = Some("save");
        assert!(transfer(&mut rows, "old", "new").is_err());
        assert_eq!(rows.sessions, before);
        assert_eq!(*rows.calls.borrow(), ["read", "save"]);
        rows.fail = None;
        rows.calls.borrow_mut().clear();
        assert!(transfer(&mut rows, "old", "new").unwrap());
        assert_eq!(*rows.calls.borrow(), ["read", "save", "delete"]);
        assert_eq!(rows.sessions.len(), 1);
        assert_eq!(rows.sessions["new"], session("new", "calm"));
    }

    #[test]
    fn failed_source_cleanup_keeps_the_new_copy_and_can_retry() {
        let mut rows = rows();
        rows.fail = Some("delete");
        assert!(transfer(&mut rows, "old", "new").is_err());
        assert_eq!(rows.sessions["old"], session("old", "calm"));
        assert_eq!(rows.sessions["new"], session("new", "calm"));
        rows.fail = None;
        assert!(transfer(&mut rows, "old", "new").unwrap());
        assert!(!rows.sessions.contains_key("old"));
        assert_eq!(rows.sessions["new"], session("new", "calm"));
    }

    #[test]
    fn self_moves_and_missing_sources_do_not_write_anything() {
        let mut rows = rows();
        let before = rows.sessions.clone();
        rows.fail = Some("save");
        assert!(transfer(&mut rows, "old", "old").unwrap());
        assert!(!transfer(&mut rows, "absent", "new").unwrap());
        assert_eq!(rows.sessions, before);
        assert_eq!(*rows.calls.borrow(), ["read", "read"]);
    }

    #[test]
    fn a_failed_read_never_changes_either_session() {
        let mut rows = rows();
        let before = rows.sessions.clone();
        rows.fail = Some("read");
        assert!(transfer(&mut rows, "old", "new").is_err());
        assert_eq!(rows.sessions, before);
        assert_eq!(*rows.calls.borrow(), ["read"]);
    }
}
