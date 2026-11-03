//! Sleep completion retains its own volume intents and device backstop over real sockets.

use chrono::{DateTime, Utc};
use fsonos_core::fade::Fader;
use fsonos_core::sleep::{SleepOutcome, SleepTimers};
use fsonos_core::{HouseholdState, control, resolve_room};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::net::IpAddr;
use std::sync::{Mutex, mpsc};
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(5);

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-07T22:00:00Z")
        .unwrap()
        .to_utc()
}

struct House {
    sim: SimHandle,
    lan: SimLan,
    houses: Vec<HouseholdState>,
    kitchen: PlayerId,
    kitchen_host: IpAddr,
    office_host: IpAddr,
}

impl House {
    fn new(s2: bool) -> Self {
        let model = if s2 {
            SimModel::One
        } else {
            SimModel::Play5Gen1
        };
        let players = [
            SimPlayerSpec::new("Kitchen", model),
            SimPlayerSpec::new("Office", model),
        ];
        let builder = SimHousehold::builder();
        let sim = if s2 {
            builder.s2(players)
        } else {
            builder.s1(players)
        }
        .spawn()
        .unwrap();
        let lan = sim.lan();
        let kitchen_host = sim.player("Kitchen").unwrap().ip;
        let office_host = sim.player("Office").unwrap().ip;
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&lan, kitchen_host).unwrap());
        let mut houses = vec![house];
        let kitchen = resolve_room(&houses, "Kitchen").unwrap().player.id.clone();
        let office = resolve_room(&houses, "Office").unwrap().player.id.clone();
        control::play_uri(
            &lan,
            &houses,
            &kitchen,
            "x-rincon-mp3radio://stream.example.invalid/sleep.mp3",
            "",
        )
        .unwrap();
        control::join(&lan, &houses, &office, &kitchen).unwrap();
        houses[0].apply_topology(&get_zone_group_state(&lan, kitchen_host).unwrap());
        soap::set_volume(&lan, kitchen_host, 30).unwrap();
        soap::set_volume(&lan, office_host, 20).unwrap();
        Self {
            sim,
            lan,
            houses,
            kitchen,
            kitchen_host,
            office_host,
        }
    }

    fn arm(&self, timers: &SleepTimers, fader: &Fader) {
        timers
            .start(
                &self.lan,
                &self.houses,
                fader,
                &self.kitchen,
                Duration::ZERO,
                now(),
            )
            .unwrap();
        assert_eq!(timers.due(now()).len(), 1);
    }

    fn levels(&self) -> (u8, u8) {
        (
            soap::get_volume(&self.lan, self.kitchen_host).unwrap(),
            soap::get_volume(&self.lan, self.office_host).unwrap(),
        )
    }

    fn transport(&self) -> TransportState {
        soap::get_transport_info(&self.lan, self.kitchen_host)
            .unwrap()
            .state
    }

    fn backstop(&self) -> Option<u32> {
        soap::get_remaining_sleep_timer(&self.lan, self.kitchen_host).unwrap()
    }
}

/// Hold one selected real request before forwarding it. Initial GetVolume can
/// instead be held after read-back, so the old snapshot is deterministic.
struct Gate<'a> {
    lan: &'a SimLan,
    host: IpAddr,
    action: &'static str,
    body: &'static str,
    after_read: bool,
    fail: bool,
    waiting: Mutex<Option<(mpsc::SyncSender<()>, mpsc::Receiver<()>)>>,
}

impl<'a> Gate<'a> {
    fn new(
        lan: &'a SimLan,
        host: IpAddr,
        action: &'static str,
        body: &'static str,
    ) -> (Self, mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (arrived, entered) = mpsc::sync_channel(1);
        let (release, waiting) = mpsc::channel();
        (
            Self {
                lan,
                host,
                action,
                body,
                after_read: false,
                fail: false,
                waiting: Mutex::new(Some((arrived, waiting))),
            },
            entered,
            release,
        )
    }
}

impl Transport for Gate<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let matches = host == self.host
            && action
                .trim_matches('"')
                .ends_with(&format!("#{}", self.action))
            && body.contains(self.body);
        let wait = if matches {
            self.waiting.lock().unwrap().take()
        } else {
            None
        };
        let Some((arrived, release)) = wait else {
            return self.lan.soap_post(host, path, action, body);
        };
        let read = self
            .after_read
            .then(|| self.lan.soap_post(host, path, action, body));
        arrived.send(()).unwrap();
        release.recv_timeout(WAIT).unwrap();
        if self.fail {
            return Err(ProtoError::Network {
                target: host.to_string(),
                detail: "injected sleep completion failure".into(),
            });
        }
        read.unwrap_or_else(|| self.lan.soap_post(host, path, action, body))
    }
}

#[test]
fn a_manual_override_during_the_last_member_step_prevents_sleep_pause() {
    for s2 in [false, true] {
        let h = House::new(s2);
        let timers = SleepTimers::new();
        let fader = Fader::default();
        h.arm(&timers, &fader);
        let from = h.sim.soap_log().len();
        let (gate, entered, release) = Gate::new(
            &h.lan,
            h.office_host,
            "SetVolume",
            "<DesiredVolume>0</DesiredVolume>",
        );
        let outcome = std::thread::scope(|scope| {
            let run = scope.spawn(|| timers.run(&gate, &h.houses, &fader, &h.kitchen));
            entered.recv_timeout(WAIT).unwrap();
            fader.set_volume(&h.lan, &h.houses, &h.kitchen, 50).unwrap();
            release.send(()).unwrap();
            run.join().unwrap().unwrap()
        });
        assert_eq!(outcome, SleepOutcome::Interrupted);
        assert_eq!(h.levels(), (50, 0));
        assert_eq!(h.transport(), TransportState::Playing);
        assert_eq!(h.backstop(), None);
        assert!(timers.get(&h.kitchen).is_none());
        assert!(
            !h.sim.soap_log()[from..]
                .iter()
                .any(|entry| entry.action == "Pause")
        );
    }
}

#[test]
fn cancel_and_extend_restore_owned_rooms_without_reclaiming_manual_changes() {
    for s2 in [false, true] {
        for extend in [false, true] {
            for change_before_cancel in [false, true] {
                let h = House::new(s2);
                let timers = SleepTimers::new();
                let fader = Fader::default();
                h.arm(&timers, &fader);
                let (mut gate, entered, release) =
                    Gate::new(&h.lan, h.kitchen_host, "GetVolume", "");
                gate.after_read = true;
                let change_volume = || fader.set_volume(&h.lan, &h.houses, &h.kitchen, 50).unwrap();
                let outcome = std::thread::scope(|scope| {
                    let run = scope.spawn(|| timers.run(&gate, &h.houses, &fader, &h.kitchen));
                    entered.recv_timeout(WAIT).unwrap();
                    if change_before_cancel {
                        change_volume();
                    }
                    if extend {
                        assert!(
                            timers
                                .extend(
                                    &h.lan,
                                    &h.houses,
                                    &fader,
                                    &h.kitchen,
                                    Duration::from_mins(10),
                                    now()
                                )
                                .unwrap()
                                .is_some()
                        );
                    } else {
                        assert!(
                            timers
                                .cancel(&h.lan, &h.houses, &fader, &h.kitchen)
                                .unwrap()
                        );
                    }
                    if !change_before_cancel {
                        change_volume();
                    }
                    release.send(()).unwrap();
                    run.join().unwrap().unwrap()
                });
                assert_eq!(outcome, SleepOutcome::Cancelled);
                assert_eq!(h.levels(), (50, 20));
                assert_eq!(h.transport(), TransportState::Playing);
                assert_eq!(timers.get(&h.kitchen).is_some(), extend);
                assert_eq!(h.backstop(), extend.then_some(15 * 60));
            }
        }
    }
}

#[test]
fn manual_changes_during_pause_wait_for_its_outcome_and_survive_restoration() {
    for s2 in [false, true] {
        for fail in [false, true] {
            let h = House::new(s2);
            let timers = SleepTimers::new();
            let fader = Fader::default();
            h.arm(&timers, &fader);
            let (mut gate, entered, release) = Gate::new(&h.lan, h.kitchen_host, "Pause", "");
            gate.fail = fail;
            let (outcome, completed_early) = std::thread::scope(|scope| {
                let run = scope.spawn(|| timers.run(&gate, &h.houses, &fader, &h.kitchen));
                entered.recv_timeout(WAIT).unwrap();
                assert!(
                    timers.get(&h.kitchen).is_some(),
                    "the run remains visible during completion"
                );
                let (done, finished) = mpsc::channel();
                let set_volume = || fader.set_volume(&h.lan, &h.houses, &h.kitchen, 50);
                let set = scope.spawn(move || {
                    let result = set_volume();
                    done.send(()).unwrap();
                    result
                });
                let early = finished.recv_timeout(Duration::from_millis(100)).is_ok();
                release.send(()).unwrap();
                set.join().unwrap().unwrap();
                (run.join().unwrap(), early)
            });
            assert!(
                !completed_early,
                "a newer write waits out a Pause already in flight"
            );
            assert_eq!(outcome.is_err(), fail);
            assert_eq!(h.levels(), (50, 20));
            assert_eq!(
                h.transport(),
                if fail {
                    TransportState::Playing
                } else {
                    TransportState::Paused
                }
            );
            assert_eq!(h.backstop(), fail.then_some(5 * 60));
            assert!(timers.get(&h.kitchen).is_none());
        }
    }
}

#[test]
fn a_manual_level_waits_out_inflight_restoration_and_other_rooms_recover_after_failure() {
    for s2 in [false, true] {
        for fail in [false, true] {
            let h = House::new(s2);
            let timers = SleepTimers::new();
            let fader = Fader::default();
            h.arm(&timers, &fader);
            let (mut gate, entered, release) = Gate::new(
                &h.lan,
                h.kitchen_host,
                "SetVolume",
                "<DesiredVolume>30</DesiredVolume>",
            );
            gate.fail = fail;
            let (outcome, completed_early) = std::thread::scope(|scope| {
                let run = scope.spawn(|| timers.run(&gate, &h.houses, &fader, &h.kitchen));
                entered.recv_timeout(WAIT).unwrap();
                let (done, finished) = mpsc::channel();
                let set_volume = || fader.set_volume(&h.lan, &h.houses, &h.kitchen, 50);
                let set = scope.spawn(move || {
                    let result = set_volume();
                    done.send(()).unwrap();
                    result
                });
                let early = finished.recv_timeout(Duration::from_millis(100)).is_ok();
                release.send(()).unwrap();
                set.join().unwrap().unwrap();
                (run.join().unwrap(), early)
            });
            assert!(
                !completed_early,
                "restoration and newer levels share the room's lock"
            );
            assert_eq!(outcome.is_err(), fail);
            assert_eq!(
                h.levels(),
                (50, 20),
                "a failed first restore does not strand another room"
            );
            assert_eq!(h.transport(), TransportState::Paused);
            assert_eq!(h.backstop(), None);
            assert!(timers.get(&h.kitchen).is_none());
        }
    }
}

#[test]
fn a_finishing_or_interrupted_run_cannot_clear_a_replacement_timers_backstop() {
    for s2 in [false, true] {
        for interrupted in [false, true] {
            let h = House::new(s2);
            let timers = SleepTimers::new();
            let fader = Fader::default();
            h.arm(&timers, &fader);
            if interrupted {
                assert!(timers.interrupt(&fader, &h.kitchen));
            }
            let (gate, entered, release) = Gate::new(
                &h.lan,
                h.kitchen_host,
                "ConfigureSleepTimer",
                "<NewSleepTimerDuration></NewSleepTimerDuration>",
            );
            let completed_early = std::thread::scope(|scope| {
                let run = scope.spawn(|| timers.run(&gate, &h.houses, &fader, &h.kitchen));
                entered.recv_timeout(WAIT).unwrap();
                let (done, finished) = mpsc::channel();
                let replace = || {
                    timers.start(
                        &h.lan,
                        &h.houses,
                        &fader,
                        &h.kitchen,
                        Duration::from_mins(10),
                        now(),
                    )
                };
                let start = scope.spawn(move || {
                    let timer = replace();
                    done.send(()).unwrap();
                    timer
                });
                let early = finished.recv_timeout(Duration::from_millis(100)).is_ok();
                release.send(()).unwrap();
                assert_eq!(
                    run.join().unwrap().unwrap(),
                    if interrupted {
                        SleepOutcome::Interrupted
                    } else {
                        SleepOutcome::Paused
                    },
                );
                start.join().unwrap().unwrap();
                early
            });
            assert!(
                !completed_early,
                "replacement waits for the previous backstop clear"
            );
            assert_eq!(h.backstop(), Some(15 * 60));
            let timer = timers
                .get(&h.kitchen)
                .expect("the replacement remains armed");
            assert_eq!(timer.ends, now() + chrono::TimeDelta::minutes(10));
            assert!(!timer.fading);
        }
    }
}

#[derive(Clone, Copy)]
enum ReArm {
    Replace,
    Extend,
    CancelThenStart,
}

#[test]
fn rearmed_timers_wait_for_retired_restoration_without_moving_their_deadlines() {
    for s2 in [false, true] {
        for action in [ReArm::Replace, ReArm::Extend, ReArm::CancelThenStart] {
            let h = House::new(s2);
            let timers = SleepTimers::new();
            let fader = Fader::default();
            h.arm(&timers, &fader);
            let (mut gate, entered, release) = Gate::new(&h.lan, h.kitchen_host, "GetVolume", "");
            gate.after_read = true;
            std::thread::scope(|scope| {
                let old = scope.spawn(|| timers.run(&gate, &h.houses, &fader, &h.kitchen));
                entered.recv_timeout(WAIT).unwrap();
                match action {
                    ReArm::Extend => {
                        timers
                            .extend(&h.lan, &h.houses, &fader, &h.kitchen, Duration::ZERO, now())
                            .unwrap()
                            .unwrap();
                    }
                    ReArm::Replace | ReArm::CancelThenStart => {
                        if matches!(action, ReArm::CancelThenStart) {
                            assert!(
                                timers
                                    .cancel(&h.lan, &h.houses, &fader, &h.kitchen)
                                    .unwrap()
                            );
                        }
                        timers
                            .start(&h.lan, &h.houses, &fader, &h.kitchen, Duration::ZERO, now())
                            .unwrap();
                    }
                }
                assert!(
                    timers.due(now()).is_empty(),
                    "the old run has not restored yet"
                );
                assert_eq!(timers.get(&h.kitchen).unwrap().ends, now());
                release.send(()).unwrap();
                assert_eq!(old.join().unwrap().unwrap(), SleepOutcome::Cancelled);
            });
            assert_eq!(timers.due(now()).len(), 1);
            assert_eq!(
                timers.run(&h.lan, &h.houses, &fader, &h.kitchen).unwrap(),
                SleepOutcome::Paused
            );
            assert_eq!(
                h.levels(),
                (30, 20),
                "the successor restores the recovered baseline"
            );
            assert_eq!(h.backstop(), None);
        }
    }
}

#[test]
fn moving_a_timer_waits_for_both_replaced_runs_across_further_rearming() {
    for s2 in [false, true] {
        let mut h = House::new(s2);
        let office = resolve_room(&h.houses, "Office").unwrap().player.id.clone();
        control::leave(&h.lan, &h.houses, &office).unwrap();
        h.houses[0].apply_topology(&get_zone_group_state(&h.lan, h.kitchen_host).unwrap());
        control::play_uri(
            &h.lan,
            &h.houses,
            &office,
            "x-rincon-mp3radio://stream.example.invalid/office.mp3",
            "",
        )
        .unwrap();
        let timers = SleepTimers::with_fade(Duration::from_millis(20));
        let fader = Fader::default();
        h.arm(&timers, &fader);
        timers
            .start(&h.lan, &h.houses, &fader, &office, Duration::ZERO, now())
            .unwrap();
        assert_eq!(timers.due(now()).len(), 1);
        let (mut kitchen_gate, kitchen_entered, kitchen_release) =
            Gate::new(&h.lan, h.kitchen_host, "GetVolume", "");
        kitchen_gate.after_read = true;
        let (mut office_gate, office_entered, office_release) =
            Gate::new(&h.lan, h.office_host, "GetVolume", "");
        office_gate.after_read = true;
        let ends = std::thread::scope(|scope| {
            let kitchen_run =
                scope.spawn(|| timers.run(&kitchen_gate, &h.houses, &fader, &h.kitchen));
            let office_run = scope.spawn(|| timers.run(&office_gate, &h.houses, &fader, &office));
            kitchen_entered.recv_timeout(WAIT).unwrap();
            office_entered.recv_timeout(WAIT).unwrap();
            assert!(
                timers
                    .rekey(&h.lan, &h.houses, &fader, &h.kitchen, &office, now())
                    .unwrap()
            );
            let ends = timers.get(&office).unwrap().ends;
            assert!(timers.due(ends).is_empty());
            kitchen_release.send(()).unwrap();
            assert_eq!(
                kitchen_run.join().unwrap().unwrap(),
                SleepOutcome::Cancelled
            );
            assert!(
                timers.due(ends).is_empty(),
                "the replaced target run is still unfinished"
            );
            timers
                .extend(&h.lan, &h.houses, &fader, &office, Duration::ZERO, ends)
                .unwrap()
                .unwrap();
            timers
                .start(&h.lan, &h.houses, &fader, &office, Duration::ZERO, ends)
                .unwrap();
            assert_eq!(timers.get(&office).unwrap().ends, ends);
            assert!(
                timers.due(ends).is_empty(),
                "repeated rearming retains the predecessor"
            );
            office_release.send(()).unwrap();
            assert_eq!(office_run.join().unwrap().unwrap(), SleepOutcome::Cancelled);
            ends
        });
        assert_eq!(timers.due(ends).len(), 1);
        assert_eq!(
            timers.run(&h.lan, &h.houses, &fader, &office).unwrap(),
            SleepOutcome::Paused
        );
        assert_eq!(h.levels(), (30, 20));
        assert_eq!(h.transport(), TransportState::Playing);
        assert_eq!(
            soap::get_transport_info(&h.lan, h.office_host)
                .unwrap()
                .state,
            TransportState::Paused
        );
    }
}

#[test]
fn an_announcement_receipt_resumes_once_and_waits_for_its_retired_worker() {
    for s2 in [false, true] {
        let h = House::new(s2);
        let timers = SleepTimers::with_fade(Duration::from_millis(20));
        let fader = Fader::default();
        h.arm(&timers, &fader);
        let (mut gate, entered, release) = Gate::new(&h.lan, h.kitchen_host, "GetVolume", "");
        gate.after_read = true;
        let receipt = std::thread::scope(|scope| {
            let old = scope.spawn(|| timers.run(&gate, &h.houses, &fader, &h.kitchen));
            entered.recv_timeout(WAIT).unwrap();
            let receipt = timers
                .interrupt_for_announcement(&fader, &h.kitchen)
                .unwrap();
            assert!(
                timers
                    .interrupt_for_announcement(&fader, &h.kitchen)
                    .is_none(),
                "another room in the same group cannot issue another receipt"
            );
            assert!(
                SleepTimers::new()
                    .resume_after_announcement(&h.lan, &h.houses, &fader, &receipt, now())
                    .unwrap()
                    .is_none(),
                "a receipt belongs only to the timers that issued it"
            );
            let resumed = timers
                .resume_after_announcement(&h.lan, &h.houses, &fader, &receipt, now())
                .unwrap()
                .unwrap();
            assert_eq!(resumed.ends, now() + chrono::TimeDelta::milliseconds(20));
            assert!(
                timers.due(now()).is_empty(),
                "announcement recovery retains the previous worker's fence"
            );
            release.send(()).unwrap();
            assert_eq!(old.join().unwrap().unwrap(), SleepOutcome::Cancelled);
            receipt
        });
        let before = h.sim.soap_log().len();
        assert!(
            timers
                .resume_after_announcement(&h.lan, &h.houses, &fader, &receipt, now())
                .unwrap()
                .is_none()
        );
        assert_eq!(
            h.sim.soap_log().len(),
            before,
            "a consumed receipt performs no I/O"
        );
        assert_eq!(timers.due(now()).len(), 1);
        assert_eq!(
            timers.run(&h.lan, &h.houses, &fader, &h.kitchen).unwrap(),
            SleepOutcome::Paused
        );
        assert_eq!(h.levels(), (30, 20));
        assert_eq!(h.backstop(), None);
    }
}

#[derive(Clone, Copy, Debug)]
enum DuringAnnouncement {
    Cancel,
    Extend,
    Replace,
    Volume,
    Move,
    Adopt,
}

#[test]
fn explicit_changes_retire_announcement_recovery_even_after_the_old_timer_is_gone() {
    for s2 in [false, true] {
        for retired in [false, true] {
            for change in [
                DuringAnnouncement::Cancel,
                DuringAnnouncement::Extend,
                DuringAnnouncement::Replace,
                DuringAnnouncement::Volume,
                DuringAnnouncement::Move,
                DuringAnnouncement::Adopt,
            ] {
                if !retired && matches!(change, DuringAnnouncement::Adopt) {
                    continue; // Adoption leaves an existing timer alone.
                }
                let mut h = House::new(s2);
                let office = resolve_room(&h.houses, "Office").unwrap().player.id.clone();
                if matches!(change, DuringAnnouncement::Move) {
                    control::leave(&h.lan, &h.houses, &office).unwrap();
                    h.houses[0]
                        .apply_topology(&get_zone_group_state(&h.lan, h.kitchen_host).unwrap());
                }
                let timers = SleepTimers::new();
                let fader = Fader::default();
                h.arm(&timers, &fader);
                let receipt = timers
                    .interrupt_for_announcement(&fader, &h.kitchen)
                    .unwrap();
                if retired {
                    assert_eq!(
                        timers.run(&h.lan, &h.houses, &fader, &h.kitchen).unwrap(),
                        SleepOutcome::Interrupted
                    );
                    assert!(timers.get(&h.kitchen).is_none());
                }
                match change {
                    DuringAnnouncement::Cancel => {
                        assert_eq!(
                            timers
                                .cancel(&h.lan, &h.houses, &fader, &h.kitchen)
                                .unwrap(),
                            !retired
                        );
                    }
                    DuringAnnouncement::Extend => {
                        assert_eq!(
                            timers
                                .extend(
                                    &h.lan,
                                    &h.houses,
                                    &fader,
                                    &h.kitchen,
                                    Duration::from_mins(10),
                                    now()
                                )
                                .unwrap()
                                .is_some(),
                            !retired
                        );
                    }
                    DuringAnnouncement::Replace => {
                        timers
                            .start(
                                &h.lan,
                                &h.houses,
                                &fader,
                                &h.kitchen,
                                Duration::from_mins(10),
                                now(),
                            )
                            .unwrap();
                    }
                    DuringAnnouncement::Volume => {
                        assert_eq!(timers.interrupt(&fader, &h.kitchen), !retired);
                        fader.set_volume(&h.lan, &h.houses, &h.kitchen, 50).unwrap();
                    }
                    DuringAnnouncement::Move => {
                        assert_eq!(
                            timers
                                .rekey(&h.lan, &h.houses, &fader, &h.kitchen, &office, now())
                                .unwrap(),
                            !retired
                        );
                    }
                    DuringAnnouncement::Adopt => {
                        assert!(timers.adopt(fsonos_core::sleep::SleepTimer {
                            coordinator: h.kitchen.clone(),
                            room: "Kitchen".into(),
                            ends: now() + chrono::TimeDelta::minutes(10),
                            fading: false,
                        }));
                    }
                }
                let timers_before = timers.all();
                let backstop_before = h.backstop();
                let calls_before = h.sim.soap_log().len();
                assert!(
                    timers
                        .resume_after_announcement(&h.lan, &h.houses, &fader, &receipt, now())
                        .unwrap()
                        .is_none(),
                    "{change:?}, retired={retired}, s2={s2}"
                );
                assert_eq!(timers.all(), timers_before);
                assert_eq!(h.sim.soap_log().len(), calls_before);
                assert_eq!(h.backstop(), backstop_before);
            }
        }
    }
}

#[test]
fn a_manual_volume_change_during_announcement_rearming_clears_its_backstop() {
    for s2 in [false, true] {
        let h = House::new(s2);
        let timers = SleepTimers::new();
        let fader = Fader::default();
        h.arm(&timers, &fader);
        let receipt = timers
            .interrupt_for_announcement(&fader, &h.kitchen)
            .unwrap();
        assert_eq!(
            timers.run(&h.lan, &h.houses, &fader, &h.kitchen).unwrap(),
            SleepOutcome::Interrupted
        );
        let (gate, entered, release) = Gate::new(&h.lan, h.kitchen_host, "ConfigureSleepTimer", "");
        std::thread::scope(|scope| {
            let resumed = scope.spawn(|| {
                timers.resume_after_announcement(&gate, &h.houses, &fader, &receipt, now())
            });
            entered.recv_timeout(WAIT).unwrap();
            assert!(!timers.interrupt(&fader, &h.kitchen));
            fader.set_volume(&h.lan, &h.houses, &h.kitchen, 50).unwrap();
            release.send(()).unwrap();
            assert!(resumed.join().unwrap().unwrap().is_none());
        });
        assert!(timers.get(&h.kitchen).is_none());
        assert_eq!(h.backstop(), None);
        assert_eq!(h.levels(), (50, 20));
        h.sim.clock().advance(Duration::from_mins(10));
        assert_eq!(h.transport(), TransportState::Playing);
    }
}

#[test]
fn a_manual_change_before_dispatch_interrupts_the_resumed_announcement_fade() {
    for s2 in [false, true] {
        let h = House::new(s2);
        let timers = SleepTimers::new();
        let fader = Fader::default();
        h.arm(&timers, &fader);
        let receipt = timers
            .interrupt_for_announcement(&fader, &h.kitchen)
            .unwrap();
        assert_eq!(
            timers.run(&h.lan, &h.houses, &fader, &h.kitchen).unwrap(),
            SleepOutcome::Interrupted
        );
        assert!(
            timers
                .resume_after_announcement(&h.lan, &h.houses, &fader, &receipt, now())
                .unwrap()
                .is_some()
        );
        assert!(!timers.get(&h.kitchen).unwrap().fading);
        assert!(timers.interrupt(&fader, &h.kitchen));
        fader.set_volume(&h.lan, &h.houses, &h.kitchen, 50).unwrap();
        assert_eq!(timers.due(now()).len(), 1);
        assert_eq!(
            timers.run(&h.lan, &h.houses, &fader, &h.kitchen).unwrap(),
            SleepOutcome::Interrupted
        );
        assert_eq!(h.levels(), (50, 20));
        assert_eq!(h.backstop(), None);
        h.sim.clock().advance(Duration::from_mins(10));
        assert_eq!(h.transport(), TransportState::Playing);
    }
}
