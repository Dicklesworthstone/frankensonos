use super::*;
use crate::zones::fixtures::households;
use fsonos_core::rooms::Aliases;
use std::sync::Barrier;
use std::sync::atomic::{AtomicUsize, Ordering};

fn timer() -> SleepTimerDto {
    SleepTimerDto {
        zone: "Room".into(),
        ends: None,
        remaining_secs: None,
        fades: false,
        fading: false,
        done: "cancelled Room's timer".into(),
        changed: true,
    }
}

#[test]
fn the_single_timer_wire_shape_is_unchanged_and_partial_sets_keep_each_identity() {
    let single = SleepOutcome::Single(timer());
    assert!(single.complete());
    assert_eq!(
        serde_json::to_value(&single).unwrap(),
        serde_json::to_value(timer()).unwrap()
    );
    assert_eq!(single.text(), "cancelled Room's timer\n");

    let failure = Failure::new(ErrorCode::NotReady, "the second group has no timer");
    let mut receipt = SleepSetDto {
        done: "one timer changed; one group refused".into(),
        complete: false,
        changed: true,
        applied: vec![SleepAppliedDto {
            household: "S1".into(),
            coordinator: "RINCON_FIRST".into(),
            timer: timer(),
        }],
        failed: vec![SleepTargetFailureDto {
            household: "S2".into(),
            coordinator: "RINCON_SECOND".into(),
            zone: "Room".into(),
            error: (&failure).into(),
        }],
    };
    let batch = SleepOutcome::Groups(receipt.clone());
    assert!(!batch.complete());
    assert!(batch.text().contains("RINCON_FIRST@S1"));
    assert!(batch.text().contains("RINCON_SECOND@S2"));
    assert!(batch.text().contains("do not replay the whole request"));
    assert_eq!(
        serde_json::from_value::<SleepOutcome>(serde_json::to_value(&batch).unwrap()).unwrap(),
        batch
    );
    receipt.complete = true;
    assert!(
        !SleepOutcome::Groups(receipt.clone()).complete(),
        "a contradictory receipt is incomplete"
    );
    receipt.failed.clear();
    assert!(SleepOutcome::Groups(receipt.clone()).complete());
    receipt.applied.clear();
    assert!(
        !SleepOutcome::Groups(receipt).complete(),
        "an empty receipt is incomplete"
    );
}

#[test]
fn preparation_deduplicates_original_groups_and_binds_the_reviewed_audience() {
    let houses = households();
    let aliases =
        Aliases::parse("[aliases]\nbedtime = ['Den', 'Kitchen@S1', 'Patio@S2', 'Den']").unwrap();
    let rooms = room_view(&houses, Some(&aliases), &Client::Cli);
    let review = resolve_view(&rooms, "bedtime").unwrap();
    let prepared = prepare(&rooms, "bedtime", Some(&review.revision)).unwrap();
    assert_eq!(prepared.groups.len(), 2);
    assert!(
        prepared
            .groups
            .iter()
            .any(|g| g.zone.coordinator.0 == "RINCON_DEN" && g.household == "S1")
    );
    let changed = Aliases::parse("[aliases]\nbedtime = ['Den', 'Kitchen@S2']").unwrap();
    let stale = prepare(
        &room_view(&houses, Some(&changed), &Client::Cli),
        "bedtime",
        Some(&review.revision),
    )
    .unwrap_err();
    assert_eq!(stale.code, ErrorCode::NotCoordinator);
    assert_eq!(
        prepare(&rooms, "bedtime", Some("not-a-revision"))
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
}

#[test]
fn independent_groups_execute_together_with_a_bound_and_ordered_results() {
    let barrier = Barrier::new(8);
    let active = AtomicUsize::new(0);
    let maximum = AtomicUsize::new(0);
    let inputs: Vec<_> = (0..16).collect();
    let results = together(&inputs, |index| {
        let running = active.fetch_add(1, Ordering::SeqCst) + 1;
        maximum.fetch_max(running, Ordering::SeqCst);
        barrier.wait();
        active.fetch_sub(1, Ordering::SeqCst);
        index * index
    });
    assert_eq!(maximum.load(Ordering::SeqCst), 8);
    assert_eq!(active.load(Ordering::SeqCst), 0);
    assert_eq!(
        results,
        inputs.into_iter().map(|n| n * n).collect::<Vec<_>>()
    );
}

#[test]
fn old_requests_remain_valid_and_optional_review_revisions_are_validated() {
    let request: SleepRequest = serde_json::from_str(r#"{"zone":"all","duration":"45m"}"#).unwrap();
    assert!(request.expected_revision.is_none());
    assert!(request.change().is_ok());
    let mut reviewed = request;
    reviewed.expected_revision = Some("0".repeat(64));
    assert!(reviewed.change().is_ok());
    reviewed.expected_revision = Some("queue-token".into());
    assert_eq!(
        reviewed.change().unwrap_err().code,
        ErrorCode::InvalidArgument
    );
    let fade: super::super::FadePauseRequest =
        serde_json::from_str(r#"{"zone":"all","fade_secs":1}"#).unwrap();
    assert!(fade.expected_revision.is_none());
}
