use super::*;
use crate::surface::testing::Canned;
use fsonos_core::clock::FakeClock;
use fsonos_core::policy::Policy;
use fsonos_core::schedule::{ScheduleAction, ScheduleSpec};
use fsonos_core::store::{MemStore, StoredSchedule};
use std::sync::mpsc;
use std::time::Duration;

fn fixture() -> (Arc<Surface>, Schedule, DateTime<Utc>) {
    let created = DateTime::parse_from_rfc3339("2026-10-01T10:00:00Z").unwrap();
    let at = created.to_utc() + chrono::TimeDelta::minutes(1);
    let surface = Arc::new(
        Surface::new(
            Box::new(Canned {
                out_args: "",
                sent: Arc::new(Mutex::new(Vec::new())),
            }),
            Box::new(|_| Ok(Vec::new())),
            Policy::default(),
            Box::new(FakeClock::new(created)),
        )
        .with_action_log(Box::new(MemStore::default()), "schedule-admission-test"),
    );
    surface
        .with_store(|store| {
            store.add_schedule(&StoredSchedule {
                id: 0,
                spec: ScheduleSpec::Once(at.fixed_offset()).to_string(),
                action: serde_json::to_string(&ScheduleAction::Pause {
                    room: "Test room".into(),
                    fade_secs: 0,
                })
                .unwrap(),
                creator: "cli".into(),
                enabled: true,
                created: created.timestamp(),
                last_fired: None,
            })
        })
        .unwrap();
    let schedule = surface.load_schedules().unwrap().remove(0);
    (surface, schedule, at)
}

fn spawn(name: String, work: Work) -> io::Result<JoinHandle<()>> {
    thread::Builder::new().name(name).spawn(work)
}

fn recorded(surface: &Surface) -> Option<DateTime<Utc>> {
    surface.load_schedules().unwrap()[0].last_fired
}

#[test]
fn failed_spawn_leaves_the_occurrence_due_and_releases_its_slot() {
    let (surface, schedule, at) = fixture();
    let scheduler = Scheduler::new(&surface);
    assert!(!launch(&scheduler, schedule.clone(), at, at, |_, _| {
        Err(io::Error::other("injected worker-start failure"))
    }));
    assert_eq!(recorded(&surface), None);
    assert!(scheduler.dispatch.active.lock().unwrap().is_empty());
    assert!(launch(&scheduler, schedule, at, at, spawn));
    assert!(scheduler.wait(Duration::from_secs(5)));
    assert_eq!(recorded(&surface), Some(at));
}

#[test]
fn bounded_admission_does_not_consume_work_which_has_no_worker_slot() {
    let (surface, schedule, at) = fixture();
    let scheduler = Scheduler::new(&surface);
    let reservations: Vec<_> = (100..100 + i64::try_from(MAX_RUNS).unwrap())
        .map(|id| scheduler.dispatch.reserve(id).unwrap())
        .collect();
    assert!(!launch(&scheduler, schedule.clone(), at, at, |_, _| {
        panic!("capacity refusal must not attempt a thread spawn")
    }));
    assert_eq!(recorded(&surface), None);
    drop(reservations);
    assert!(launch(&scheduler, schedule, at, at, spawn));
    assert!(scheduler.wait(Duration::from_secs(5)));
    assert_eq!(recorded(&surface), Some(at));
}

#[test]
fn repeated_ticks_cannot_allocate_two_workers_for_one_schedule() {
    let (surface, schedule, at) = fixture();
    let scheduler = Scheduler::new(&surface);
    let (release, wait) = mpsc::channel();
    assert!(launch(&scheduler, schedule.clone(), at, at, |name, work| {
        spawn(
            name,
            Box::new(move || {
                wait.recv().unwrap();
                work();
            }),
        )
    }));
    assert_eq!(
        recorded(&surface),
        None,
        "allocation is not durable admission"
    );
    assert!(!launch(&scheduler, schedule, at, at, |_, _| {
        panic!("the first worker still owns this occurrence")
    }));
    release.send(()).unwrap();
    assert!(scheduler.wait(Duration::from_secs(5)));
    assert_eq!(recorded(&surface), Some(at));
}

#[test]
fn pausing_before_worker_admission_prevents_execution_from_its_old_snapshot() {
    let (surface, schedule, at) = fixture();
    let scheduler = Scheduler::new(&surface);
    let (release, wait) = mpsc::channel();
    let id = schedule.id;
    assert!(launch(&scheduler, schedule, at, at, |name, work| {
        spawn(
            name,
            Box::new(move || {
                wait.recv().unwrap();
                work();
            }),
        )
    }));
    surface
        .with_store(|store| store.set_schedule_enabled(id, false))
        .unwrap();
    release.send(()).unwrap();
    assert!(scheduler.wait(Duration::from_secs(5)));
    assert_eq!(recorded(&surface), None);
}

#[test]
fn removed_and_resumed_schedules_do_not_accept_an_old_claim() {
    let (surface, schedule, at) = fixture();
    surface
        .with_store(|store| store.mark_schedule_fired(schedule.id, at.timestamp()))
        .unwrap();
    assert_eq!(
        claim_current(&surface, &schedule, at, || Admission::Run).unwrap(),
        Admission::Hold
    );
    surface
        .with_store(|store| store.delete_schedule(schedule.id))
        .unwrap();
    assert_eq!(
        claim_current(&surface, &schedule, at, || Admission::Run).unwrap(),
        Admission::Hold
    );
}

#[test]
fn changed_definitions_and_creators_are_not_admitted_under_old_rights() {
    let (surface, original, at) = fixture();
    for change in 0..3 {
        let mut stale = original.clone();
        match change {
            0 => stale.creator = "unknown".into(),
            1 => {
                stale.action = ScheduleAction::Volume {
                    room: "Other room".into(),
                    level: 10,
                };
            }
            _ => stale.created += chrono::TimeDelta::seconds(1),
        }
        assert_eq!(
            claim_current(&surface, &stale, at, || Admission::Run).unwrap(),
            Admission::Hold
        );
    }
    assert_eq!(recorded(&surface), None);
}

#[test]
fn shutdown_before_admission_keeps_work_due_for_a_later_daemon() {
    let (surface, schedule, at) = fixture();
    let scheduler = Scheduler::new(&surface);
    let (release, wait) = mpsc::channel();
    assert!(launch(&scheduler, schedule.clone(), at, at, |name, work| {
        spawn(
            name,
            Box::new(move || {
                wait.recv().unwrap();
                work();
            }),
        )
    }));
    scheduler.dispatch.stop();
    release.send(()).unwrap();
    assert!(scheduler.wait(Duration::from_secs(5)));
    assert_eq!(recorded(&surface), None);
    assert!(scheduler.tick_at(at, &Utc).is_empty());
    let next = Scheduler::new(&surface);
    assert!(launch(&next, schedule, at, at, spawn));
    assert!(next.wait(Duration::from_secs(5)));
    assert_eq!(recorded(&surface), Some(at));
}

#[test]
fn waiting_past_grace_records_a_skip_without_starting_the_action() {
    let (surface, schedule, at) = fixture();
    let scheduler = Scheduler::new(&surface);
    let late = at + chrono::TimeDelta::minutes(11);
    assert!(launch(&scheduler, schedule, at, late, spawn));
    assert!(scheduler.wait(Duration::from_secs(5)));
    assert_eq!(recorded(&surface), Some(at));
    assert!(
        surface
            .recent_actions(
                &fsonos_core::policy::Client::Cli,
                &fsonos_core::store::ActionFilter::default(),
            )
            .unwrap()
            .is_empty()
    );
}

#[test]
fn concurrent_durable_claims_have_only_one_winner() {
    let (surface, schedule, at) = fixture();
    let claims: Vec<_> = (0..8)
        .map(|_| {
            let surface = Arc::clone(&surface);
            let schedule = schedule.clone();
            thread::spawn(move || {
                claim_current(&surface, &schedule, at, || Admission::Run).unwrap()
            })
        })
        .collect();
    let winners = claims
        .into_iter()
        .map(|claim| claim.join().unwrap())
        .filter(|admission| *admission == Admission::Run)
        .count();
    assert_eq!(winners, 1);
    assert_eq!(recorded(&surface), Some(at));
}

#[test]
fn unwinding_releases_an_unstarted_reservation_without_consuming_the_schedule() {
    let (surface, schedule, _) = fixture();
    let dispatch = Arc::new(Dispatch::default());
    let cloned = Arc::clone(&dispatch);
    let id = schedule.id;
    assert!(
        thread::spawn(move || {
            let _reservation = cloned.reserve(id).unwrap();
            panic!("injected pre-admission failure");
        })
        .join()
        .is_err()
    );
    assert!(dispatch.reserve(id).is_some());
    assert_eq!(recorded(&surface), None);
}
