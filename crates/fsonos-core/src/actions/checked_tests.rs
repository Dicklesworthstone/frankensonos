use super::*;
use crate::store::{ActionFilter, MemStore};

struct NoLan;
impl Transport for NoLan {
    fn soap_post(
        &self,
        _: std::net::IpAddr,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<String, fsonos_proto::ProtoError> {
        panic!("selection fixtures must not touch speakers")
    }
}

fn add(store: &mut MemStore, client: &Client, at: i64, before: Option<&str>) -> i64 {
    record(
        store,
        &Action {
            at,
            client: client.key().into(),
            surface: "test".into(),
            intent: format!("synthetic action {at}"),
            decision: "allow".into(),
            result: "recorded".into(),
            before_state: before.map(str::to_string),
            undo_of: None,
        },
    )
    .unwrap()
}

fn checked(store: &mut MemStore, expected: i64) -> Result<UndoReport, CheckedUndoError> {
    undo_last_vetted_checked(
        &NoLan,
        &[],
        store,
        Some(&Client::Cli),
        &Client::Cli,
        "test",
        100,
        expected,
        &|op| Ok(op.clone()),
    )
}

#[test]
fn a_missing_or_ineligible_target_is_refused_without_a_log_write() {
    let mut store = MemStore::default();
    let id = add(&mut store, &Client::Cli, 1, None);
    assert!(matches!(checked(&mut store, id),
        Err(CheckedUndoError::TargetChanged { expected, actual: None }) if expected == id));
    assert_eq!(
        store
            .recent_actions(&ActionFilter::default())
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn a_newer_action_is_not_silently_substituted_for_the_reviewed_one() {
    let mut store = MemStore::default();
    let first = add(&mut store, &Client::Cli, 1, Some("[]"));
    // An unreadable newer snapshot proves rejection precedes snapshot parsing.
    let newer = add(&mut store, &Client::Cli, 2, Some("broken"));
    assert!(matches!(checked(&mut store, first),
        Err(CheckedUndoError::TargetChanged { actual: Some(id), .. }) if id == newer));
    assert_eq!(
        store
            .recent_actions(&ActionFilter::default())
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        store.last_undoable_action(Some("cli")).unwrap().unwrap().id,
        newer
    );
}

#[test]
fn another_clients_newer_action_does_not_change_the_own_action_scope() {
    let mut store = MemStore::default();
    let own = add(&mut store, &Client::Cli, 1, Some("[]"));
    let other = add(&mut store, &Client::McpStdio, 2, Some("[]"));
    assert!(matches!(checked(&mut store, other),
        Err(CheckedUndoError::TargetChanged { actual: Some(id), .. }) if id == own));
    let report = checked(&mut store, own).unwrap();
    assert_eq!(report.undone, own);
    assert_eq!(store.last_undoable_action(None).unwrap().unwrap().id, other);
}

#[test]
fn a_repeated_confirmation_cannot_step_back_to_an_older_action() {
    let mut store = MemStore::default();
    let older = add(&mut store, &Client::Cli, 1, Some("[]"));
    let latest = add(&mut store, &Client::Cli, 2, Some("[]"));
    let report = checked(&mut store, latest).unwrap();
    assert_eq!(report.undone, latest);
    assert!(matches!(checked(&mut store, latest),
        Err(CheckedUndoError::TargetChanged { actual: Some(id), .. }) if id == older));
    let log = store.recent_actions(&ActionFilter::default()).unwrap();
    assert_eq!(log.iter().filter(|r| r.action.undo_of.is_some()).count(), 1);
    // Legacy undo still uses the identical restore implementation.
    let report = undo_last(&NoLan, &[], &mut store, None, &Client::Cli, "test", 101)
        .unwrap()
        .unwrap();
    assert_eq!(report.undone, older);
}

#[test]
fn an_unreadable_selected_snapshot_is_a_core_failure_not_a_stale_review() {
    let mut store = MemStore::default();
    let id = add(&mut store, &Client::Cli, 1, Some("broken"));
    assert!(matches!(
        checked(&mut store, id),
        Err(CheckedUndoError::Core(_))
    ));
    assert_eq!(store.last_undoable_action(None).unwrap().unwrap().id, id);
}
