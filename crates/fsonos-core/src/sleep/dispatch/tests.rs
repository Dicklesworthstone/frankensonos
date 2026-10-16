//! Admission identity, remaining deadlines, and real simulator execution.

use super::*;
use chrono::TimeDelta;
use fsonos_proto::ProtoError;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHousehold, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::net::IpAddr;
use std::sync::{Mutex, mpsc};
use std::time::Duration;

fn epoch() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-07T22:00:00Z")
        .unwrap()
        .to_utc()
}

fn timer(name: &str, after: i64) -> SleepTimer {
    SleepTimer {
        coordinator: PlayerId(name.into()),
        room: name.into(),
        ends: epoch() + TimeDelta::seconds(after),
        fading: false,
    }
}

struct NoIo;

impl Transport for NoIo {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("obsolete dispatch must perform no speaker I/O")
    }
}

#[test]
fn capacity_claims_only_the_earliest_timers_and_leaves_the_rest_armed() {
    let timers = SleepTimers::new();
    for (name, after) in [("late", 90), ("early", 5), ("middle", 30)] {
        assert!(timers.adopt(timer(name, after)));
    }
    assert!(timers.due_runs(epoch(), 0).is_empty());
    assert!(timers.all().iter().all(|timer| !timer.fading));
    let jobs = timers.due_runs(epoch(), 2);
    assert_eq!(
        jobs.iter()
            .map(|job| job.timer().room.as_str())
            .collect::<Vec<_>>(),
        ["early", "middle"]
    );
    assert!(!timers.get(&PlayerId("late".into())).unwrap().fading);
    assert_eq!(timers.due_runs(epoch(), 2)[0].timer().room, "late");
    assert!(timers.due_runs(epoch(), 10).is_empty());
}

#[test]
fn failed_admission_returns_the_same_timer_without_adding_fade_time() {
    let timers = SleepTimers::new();
    let original = timer("room", 60);
    assert!(timers.adopt(original.clone()));
    let job = timers.due_runs(epoch(), 1).remove(0);
    let deadline = timers.lock()[&original.coordinator].fade_deadline;
    assert!(timers.release_run(&job));
    assert!(!timers.release_run(&job));
    assert_eq!(timers.get(&original.coordinator), Some(original.clone()));
    // A wall clock moving backward may not buy this dispatch extra fade time.
    let next = timers
        .due_runs(epoch() - TimeDelta::seconds(30), 1)
        .remove(0);
    assert_eq!(next.timer().ends, original.ends);
    assert_eq!(timers.lock()[&original.coordinator].fade_deadline, deadline);
    assert!(timers.release_run(&next));
    let overdue = timers
        .due_runs(epoch() + TimeDelta::minutes(5), 1)
        .remove(0);
    assert!(timers.lock()[&original.coordinator].fade_deadline.unwrap() <= Instant::now());
    assert_eq!(overdue.timer().ends, original.ends);
}

#[test]
fn an_old_release_or_worker_cannot_consume_a_new_dispatch_of_the_same_timer() {
    let timers = SleepTimers::new();
    assert!(timers.adopt(timer("room", 30)));
    let old = timers.due_runs(epoch(), 1).remove(0);
    assert!(timers.release_run(&old));
    let new = timers.due_runs(epoch(), 1).remove(0);
    assert!(!timers.release_run(&old));
    assert_eq!(
        timers.run_due(&NoIo, &[], &Fader::default(), &old).unwrap(),
        SleepOutcome::Gone
    );
    assert!(timers.get(&new.timer().coordinator).unwrap().fading);
    assert!(timers.release_run(&new));
}

#[test]
fn replacing_a_timer_with_identical_public_fields_still_invalidates_old_work() {
    let timers = SleepTimers::new();
    let original = timer("room", 30);
    assert!(timers.adopt(original.clone()));
    let old = timers.due_runs(epoch(), 1).remove(0);
    let _ = timers.lock().remove(&original.coordinator);
    assert!(timers.adopt(original));
    let new = timers.due_runs(epoch(), 1).remove(0);
    assert_eq!(old.timer(), new.timer());
    assert!(!timers.release_run(&old));
    assert_eq!(
        timers.run_due(&NoIo, &[], &Fader::default(), &old).unwrap(),
        SleepOutcome::Gone
    );
    assert!(timers.release_run(&new));
}

#[test]
fn cancellation_cannot_be_reversed_by_releasing_an_old_dispatch() {
    let timers = SleepTimers::new();
    let original = timer("room", 30);
    assert!(timers.adopt(original.clone()));
    let job = timers.due_runs(epoch(), 1).remove(0);
    let _ = timers.lock().remove(&original.coordinator);
    assert!(!timers.release_run(&job));
    assert_eq!(
        timers.run_due(&NoIo, &[], &Fader::default(), &job).unwrap(),
        SleepOutcome::Gone
    );
    assert!(timers.all().is_empty());
}

#[test]
fn interruption_survives_failed_admission_and_volume_changes_while_waiting() {
    for before_release in [false, true] {
        let timers = SleepTimers::new();
        let original = timer("room", 30);
        assert!(timers.adopt(original.clone()));
        let job = timers.due_runs(epoch(), 1).remove(0);
        let fader = Fader::default();
        if before_release {
            assert!(timers.interrupt(&fader, &original.coordinator));
        }
        assert!(timers.release_run(&job));
        if !before_release {
            assert!(timers.interrupt(&fader, &original.coordinator));
        }
        let _next = timers.due_runs(epoch(), 1).remove(0);
        assert!(timers.lock()[&original.coordinator].interrupted);
    }
}

#[test]
fn running_work_cannot_be_released_or_entered_twice_even_through_legacy_run() {
    let timers = SleepTimers::new();
    let original = timer("room", 30);
    assert!(timers.adopt(original.clone()));
    let job = timers.due_runs(epoch(), 1).remove(0);
    timers
        .lock()
        .get_mut(&original.coordinator)
        .unwrap()
        .running = true;
    assert!(!timers.release_run(&job));
    assert_eq!(
        timers.run_due(&NoIo, &[], &Fader::default(), &job).unwrap(),
        SleepOutcome::Gone
    );
    assert_eq!(
        timers
            .run(&NoIo, &[], &Fader::default(), &original.coordinator)
            .unwrap(),
        SleepOutcome::Gone
    );
    assert!(timers.get(&original.coordinator).is_some());
}

struct HoldVolume<'a> {
    inner: &'a (dyn Transport + Sync),
    wait: Mutex<Option<(mpsc::SyncSender<()>, mpsc::Receiver<()>)>>,
}

impl Transport for HoldVolume<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let reply = self.inner.soap_post(host, path, action, body)?;
        if action.contains("#GetVolume") {
            let wait = self.wait.lock().unwrap().take();
            if let Some((arrived, release)) = wait {
                arrived.send(()).unwrap();
                release.recv_timeout(Duration::from_secs(5)).unwrap();
            }
        }
        Ok(reply)
    }
}

#[test]
fn real_s1_and_s2_workers_pause_and_restore_once_despite_duplicate_dispatch() {
    for s2 in [false, true] {
        let spec = SimPlayerSpec::new(
            "Room",
            if s2 {
                SimModel::One
            } else {
                SimModel::Play5Gen1
            },
        );
        let builder = SimHousehold::builder();
        let sim = if s2 {
            builder.s2([spec])
        } else {
            builder.s1([spec])
        }
        .spawn()
        .unwrap();
        let lan = sim.lan();
        let host = sim.player("Room").unwrap().ip;
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
        let houses = [house];
        let id = crate::resolve_room(&houses, "Room")
            .unwrap()
            .player
            .id
            .clone();
        crate::control::play_uri(
            &lan,
            &houses,
            &id,
            "x-rincon-mp3radio://stream.example.invalid/sleep.mp3",
            "",
        )
        .unwrap();
        crate::control::set_volume(&lan, &houses, &id, 30).unwrap();
        let timers = SleepTimers::new();
        let fader = Fader::default();
        timers
            .start(&lan, &houses, &fader, &id, Duration::ZERO, epoch())
            .unwrap();
        let job = timers.due_runs(epoch(), 1).remove(0);
        let (arrived, entered) = mpsc::sync_channel(1);
        let (release, waiting) = mpsc::channel();
        let held = HoldVolume {
            inner: &lan,
            wait: Mutex::new(Some((arrived, waiting))),
        };
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| timers.run_due(&held, &houses, &fader, &job));
            entered.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(!timers.release_run(&job));
            assert_eq!(
                timers.run_due(&lan, &houses, &fader, &job).unwrap(),
                SleepOutcome::Gone
            );
            release.send(()).unwrap();
            assert_eq!(worker.join().unwrap().unwrap(), SleepOutcome::Paused);
        });
        assert_eq!(fsonos_proto::control::get_volume(&lan, host).unwrap(), 30);
        assert_eq!(
            fsonos_proto::control::get_transport_info(&lan, host)
                .unwrap()
                .state,
            TransportState::Paused
        );
        assert_eq!(
            sim.soap_log()
                .iter()
                .filter(|entry| entry.action == "Pause")
                .count(),
            1
        );
        assert!(timers.due_runs(epoch(), 1).is_empty());
    }
}
