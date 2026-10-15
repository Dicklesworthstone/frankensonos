//! Own-action preview and checked undo, sharing the legacy undo's locks.

use super::super::{Surface, vet_restore};
use super::SharedStore;
use crate::failure::{ErrorCode, Failure};
use crate::log::{ActionDto, UndoDto};
use fsonos_core::actions::{self, CheckedUndoError};
use fsonos_core::policy::Client;
use fsonos_core::snapshot::RestoreOp;
use fsonos_core::store::Store;
use std::sync::PoisonError;

impl Surface {
    /// The caller's newest eligible action, without touching the speakers.
    /// This is an observation, not a reservation or a grant of permission.
    pub fn preview_undo(&self, client: &Client) -> Result<Option<ActionDto>, Failure> {
        self.guard(client).authorize("recent_actions", true)?;
        let log = self
            .log
            .as_ref()
            .ok_or_else(|| Failure::new(ErrorCode::NotImplemented, "undo needs the action log"))?;
        // Do not preview a target while another undo is still consuming it.
        let _undoing = log.undoing.lock().unwrap_or_else(PoisonError::into_inner);
        let selected = SharedStore(&log.store)
            .last_undoable_action(Some(client.key()))
            .map_err(|e| Failure::new(ErrorCode::Internal, e.to_string()))?;
        Ok(selected.as_ref().map(ActionDto::from))
    }

    /// Restore exactly the caller's reviewed newest action. Even CLI callers
    /// use their own scope here; the legacy CLI undo-anyone remains separate.
    /// A mismatch is INVALID_ARGUMENT, non-retryable, and restores nothing.
    pub fn undo_checked(&self, client: &Client, expected_id: i64) -> Result<UndoDto, Failure> {
        if expected_id <= 0 {
            return Err(Failure::invalid("expected_id must be a positive action id"));
        }
        let log = self
            .log
            .as_ref()
            .ok_or_else(|| Failure::new(ErrorCode::NotImplemented, "undo needs the action log"))?;
        let guard = self.guard(client);
        if let Err(denied) = guard.authorize("undo_last", false) {
            let detail = denied.detail.clone();
            self.record(
                client,
                format!("undo_last: expected #{expected_id}"),
                format!("deny: {detail}"),
                detail,
                None,
            );
            return Err(denied);
        }
        self.invalidate();
        let households = self.households()?;
        let vet = |op: &RestoreOp| {
            let vetted = vet_restore(&guard, &*self.transport, &households, op);
            if let Ok(RestoreOp::SetVolume { player, .. }) = &vetted {
                self.volumes_changing(&households, std::slice::from_ref(player));
            }
            vetted
        };
        // Match the legacy lock order. Store calls stay short: never hold its
        // mutex across network I/O, and keep DJ top-ups out during restore.
        let _undoing = log.undoing.lock().unwrap_or_else(PoisonError::into_inner);
        let _restoring = log
            .restoring
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let report = actions::undo_last_vetted_checked(
            &*self.transport,
            &households,
            &mut SharedStore(&log.store),
            Some(client),
            client,
            &log.surface,
            self.now(),
            expected_id,
            &vet,
        );
        self.invalidate();
        match report {
            Ok(report) => Ok(UndoDto::from(Some(report))),
            Err(CheckedUndoError::Core(error)) => Err(error.into()),
            Err(error @ CheckedUndoError::TargetChanged { .. }) => {
                Err(Failure::invalid(error.to_string()).with_hint(
                    "Read GET /undo/preview and review the current action before confirming again. No action was restored.",
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::surface::ActionLog;
    use fsonos_core::clock::FakeClock;
    use fsonos_core::policy::Policy;
    use fsonos_core::store::{Action, ActionFilter, MemStore};
    use fsonos_proto::{ProtoError, Transport};
    use std::sync::{Arc, Barrier, Mutex, RwLock};

    struct NoLan;
    impl Transport for NoLan {
        fn soap_post(
            &self,
            _: std::net::IpAddr,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<String, ProtoError> {
            panic!("empty-snapshot tests must not contact speakers")
        }
    }

    fn surface() -> Surface {
        let mut surface = Surface::new(
            Box::new(NoLan),
            Box::new(|_| Ok(Vec::new())),
            Policy::default(),
            Box::new(FakeClock::new(
                chrono::DateTime::parse_from_rfc3339("1970-01-01T00:01:40Z").unwrap(),
            )),
        );
        surface.log = Some(ActionLog {
            store: Mutex::new(Box::new(MemStore::default())),
            surface: "test".into(),
            undoing: Mutex::new(()),
            restoring: RwLock::new(()),
        });
        surface
    }

    fn add(s: &Surface, client: &Client, at: i64) -> i64 {
        let mut store = SharedStore(&s.log.as_ref().unwrap().store);
        store
            .record_action(&Action {
                at,
                client: client.key().into(),
                surface: "test".into(),
                intent: format!("synthetic action {at}"),
                decision: "allow".into(),
                result: "recorded".into(),
                before_state: Some("[]".into()),
                undo_of: None,
            })
            .unwrap()
    }

    fn undo_count(s: &Surface) -> usize {
        SharedStore(&s.log.as_ref().unwrap().store)
            .recent_actions(&ActionFilter::default())
            .unwrap()
            .iter()
            .filter(|row| row.action.undo_of.is_some())
            .count()
    }

    #[test]
    fn preview_is_read_only_and_never_selects_another_clients_action() {
        let s = surface();
        assert!(s.preview_undo(&Client::LoopbackHttp).unwrap().is_none());
        let own = add(&s, &Client::LoopbackHttp, 1);
        let other = add(&s, &Client::Cli, 2);
        let preview = s.preview_undo(&Client::LoopbackHttp).unwrap().unwrap();
        assert_eq!(preview.id, own);
        assert_eq!(preview.client, "loopback-http");
        assert_eq!(undo_count(&s), 0);
        let failure = s.undo_checked(&Client::LoopbackHttp, other).unwrap_err();
        assert_eq!(failure.code, ErrorCode::InvalidArgument);
        assert_eq!(undo_count(&s), 0);
        assert_eq!(
            s.undo_checked(&Client::LoopbackHttp, own).unwrap().undone,
            Some(own)
        );
        assert_eq!(s.preview_undo(&Client::Cli).unwrap().unwrap().id, other);
    }

    #[test]
    fn cli_checked_undo_also_remains_own_only() {
        let s = surface();
        let other = add(&s, &Client::McpStdio, 1);
        assert!(s.preview_undo(&Client::Cli).unwrap().is_none());
        assert_eq!(
            s.undo_checked(&Client::Cli, other).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        assert_eq!(undo_count(&s), 0);
    }

    #[test]
    fn a_new_action_requires_another_review_and_legacy_undo_still_works() {
        let s = surface();
        let own = add(&s, &Client::Cli, 1);
        assert_eq!(s.preview_undo(&Client::Cli).unwrap().unwrap().id, own);
        let newer = add(&s, &Client::Cli, 2);
        let failure = s.undo_checked(&Client::Cli, own).unwrap_err();
        assert_eq!(failure.code, ErrorCode::InvalidArgument);
        assert!(!failure.code.retryable());
        assert_eq!(undo_count(&s), 0);
        assert_eq!(s.undo(&Client::Cli, true).unwrap().unwrap().undone, newer);
        assert_eq!(s.undo_checked(&Client::Cli, own).unwrap().undone, Some(own));
    }

    #[test]
    fn denied_and_invalid_requests_do_not_restore_anything() {
        let s = surface();
        let own = add(&s, &Client::Unknown, 1);
        assert_eq!(
            s.undo_checked(&Client::Unknown, own).unwrap_err().code,
            ErrorCode::PolicyDenied
        );
        for id in [0, -1] {
            assert_eq!(
                s.undo_checked(&Client::Cli, id).unwrap_err().code,
                ErrorCode::InvalidArgument
            );
        }
        assert_eq!(undo_count(&s), 0);
        let rows = SharedStore(&s.log.as_ref().unwrap().store)
            .recent_actions(&ActionFilter::default())
            .unwrap();
        assert!(
            rows.iter()
                .any(|row| row.action.decision.starts_with("deny:"))
        );
    }

    #[test]
    fn simultaneous_confirmations_consume_the_action_only_once() {
        let s = Arc::new(surface());
        let own = add(&s, &Client::Cli, 1);
        let barrier = Arc::new(Barrier::new(3));
        let workers: Vec<_> = (0..2)
            .map(|_| {
                let s = Arc::clone(&s);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    s.undo_checked(&Client::Cli, own)
                })
            })
            .collect();
        barrier.wait();
        let results: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        assert_eq!(undo_count(&s), 1);
    }
}
