//! Deterministic worker admission, with real simulator SOAP for execution.

use super::super::Surface;
use super::*;
use crate::failure::{ErrorCode, Failure};
use crate::surface::Survey;
use fsonos_core::HouseholdState;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::Policy;
use fsonos_core::sleep::SleepTimer;
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::net::IpAddr;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

struct Replies {
    lan: SimLan,
    lose_pause: Arc<AtomicBool>,
}

impl Transport for Replies {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let reply = self.lan.soap_post(host, path, action, body)?;
        if action.trim_matches('"').ends_with("#Pause")
            && self.lose_pause.swap(false, Ordering::AcqRel)
        {
            return Err(ProtoError::Network {
                target: "synthetic-sleep-player".into(),
                detail: "accepted pause but lost the reply".into(),
            });
        }
        Ok(reply)
    }
}

struct House {
    sim: SimHandle,
    surface: Arc<Surface>,
    houses: Vec<HouseholdState>,
    inventory: Arc<Mutex<Result<Vec<HouseholdState>, Failure>>>,
    lose_pause: Arc<AtomicBool>,
    now: DateTime<Utc>,
}

impl House {
    fn new(count: usize, s2: bool) -> Self {
        let specs: Vec<_> = (0..count)
            .map(|n| {
                SimPlayerSpec::new(
                    &format!("Room{n}"),
                    if s2 {
                        SimModel::One
                    } else {
                        SimModel::Play5Gen1
                    },
                )
            })
            .collect();
        let builder = SimHousehold::builder();
        let sim = if s2 {
            builder.s2(specs)
        } else {
            builder.s1(specs)
        }
        .spawn()
        .unwrap();
        let lan = sim.lan();
        let mut household = HouseholdState::default();
        household
            .apply_topology(&get_zone_group_state(&lan, sim.player("Room0").unwrap().ip).unwrap());
        let houses = vec![household];
        let inventory = Arc::new(Mutex::new(Ok(houses.clone())));
        let known = Arc::clone(&inventory);
        let survey: Survey = Box::new(move |_| known.lock().unwrap().clone());
        let lose_pause = Arc::new(AtomicBool::new(false));
        let surface = Arc::new(
            Surface::new(
                Box::new(Replies {
                    lan,
                    lose_pause: Arc::clone(&lose_pause),
                }),
                survey,
                Policy::default(),
                Box::new(SystemClock),
            )
            .with_sleep(Sleep::default()),
        );
        let h = Self {
            sim,
            surface,
            houses,
            inventory,
            lose_pause,
            now: Utc::now(),
        };
        for n in 0..count {
            let room = format!("Room{n}");
            assert!(h.sleep().timers.adopt(SleepTimer {
                coordinator: h.id(&room),
                room,
                ends: h.now,
                fading: false,
            }));
        }
        h
    }

    fn sleep(&self) -> &Arc<Sleep> {
        self.surface.sleep.as_ref().unwrap()
    }

    fn inventory(&self, next: Result<Vec<HouseholdState>, Failure>) {
        *self.inventory.lock().unwrap() = next;
        self.surface.invalidate();
    }

    fn id(&self, room: &str) -> PlayerId {
        fsonos_core::resolve_room(&self.houses, room)
            .unwrap()
            .player
            .id
            .clone()
    }

    fn play(&self) {
        let id = self.id("Room0");
        fsonos_core::control::play_uri(
            &self.sim.lan(),
            &self.houses,
            &id,
            "x-rincon-mp3radio://stream.example.invalid/sleep.mp3",
            "",
        )
        .unwrap();
        fsonos_core::control::set_volume(&self.sim.lan(), &self.houses, &id, 30).unwrap();
    }
}

#[test]
fn unavailable_inventory_retains_the_original_timer_until_its_room_returns() {
    for s2 in [false, true] {
        for unavailable in [
            Err(Failure::new(ErrorCode::NotReady, "survey is still running")),
            Ok(Vec::new()),
            Ok(crate::zones::fixtures::households()),
        ] {
            let h = House::new(1, s2);
            h.play();
            let id = h.id("Room0");
            let original = h.sleep().timers.get(&id).unwrap();
            h.inventory(unavailable);
            let scheduler = Scheduler::new(&h.surface);
            let before = h.sim.soap_log().len();
            for _ in 0..2 {
                assert_eq!(start(&scheduler, h.now).len(), 1);
                assert!(scheduler.wait(Duration::from_secs(5)));
                assert_eq!(h.sleep().timers.get(&id), Some(original.clone()));
                assert_eq!(h.sim.soap_log().len(), before);
            }
            h.inventory(Ok(h.houses.clone()));
            assert_eq!(
                start(&scheduler, h.now + chrono::TimeDelta::minutes(2)).len(),
                1
            );
            assert!(scheduler.wait(Duration::from_secs(5)));
            assert!(h.sleep().timers.get(&id).is_none());
            assert!(start(&scheduler, h.now).is_empty());
            assert_eq!(
                h.sim
                    .soap_log()
                    .iter()
                    .filter(|e| e.action == "Pause")
                    .count(),
                1
            );
            let host = h.sim.player("Room0").unwrap().ip;
            assert_eq!(soap::get_volume(&h.sim.lan(), host).unwrap(), 30);
            assert_eq!(
                soap::get_transport_info(&h.sim.lan(), host).unwrap().state,
                TransportState::Paused
            );
        }
    }
}

#[test]
fn a_long_outage_expires_unstarted_sleep_without_pausing_later_music() {
    for s2 in [false, true] {
        let h = House::new(1, s2);
        h.play();
        h.inventory(Err(Failure::new(ErrorCode::NotReady, "LAN unavailable")));
        let scheduler = Scheduler::new(&h.surface);
        let before = h.sim.soap_log().len();
        assert_eq!(start(&scheduler, h.now).len(), 1);
        assert!(scheduler.wait(Duration::from_secs(5)));
        assert!(!h.sleep().timers.all().is_empty());
        let late = h.now + chrono::TimeDelta::minutes(6);
        assert_eq!(start(&scheduler, late).len(), 1);
        assert!(scheduler.wait(Duration::from_secs(5)));
        assert!(h.sleep().timers.all().is_empty());
        assert_eq!(h.sim.soap_log().len(), before);
        h.inventory(Ok(h.houses.clone()));
        assert!(start(&scheduler, late).is_empty());
        assert_eq!(
            soap::get_transport_info(&h.sim.lan(), h.sim.player("Room0").unwrap().ip)
                .unwrap()
                .state,
            TransportState::Playing
        );
    }
}

#[test]
fn a_failed_spawn_leaves_the_timer_due_and_reports_no_fade() {
    for s2 in [false, true] {
        let h = House::new(1, s2);
        h.play();
        let scheduler = Scheduler::new(&h.surface);
        let before = h.sim.soap_log().len();
        let ran = start_with(&scheduler, h.now, |_work| false);
        assert!(ran.is_empty());
        assert_eq!(h.sleep().fades_in_flight.load(Ordering::Acquire), 0);
        assert!(!h.sleep().timers.get(&h.id("Room0")).unwrap().fading);
        assert_eq!(h.sim.soap_log().len(), before);
        assert_eq!(start(&scheduler, h.now).len(), 1);
        assert!(scheduler.wait(Duration::from_secs(5)));
        let host = h.sim.player("Room0").unwrap().ip;
        assert_eq!(
            soap::get_transport_info(&h.sim.lan(), host).unwrap().state,
            TransportState::Paused
        );
        assert_eq!(soap::get_volume(&h.sim.lan(), host).unwrap(), 30);
        assert_eq!(
            h.sim
                .soap_log()
                .iter()
                .filter(|entry| entry.action == "Pause")
                .count(),
            1
        );
        assert!(start(&scheduler, h.now).is_empty());
    }
}

#[test]
fn outstanding_workers_are_bounded_and_excess_timers_remain_armed() {
    let h = House::new(MAX_FADES + 3, true);
    let scheduler = Scheduler::new(&h.surface);
    let mut queued = Vec::new();
    let ran = start_with(&scheduler, h.now, |work| {
        queued.push(work);
        true
    });
    assert_eq!(ran.len(), MAX_FADES);
    assert_eq!(h.sleep().fades_in_flight.load(Ordering::Acquire), MAX_FADES);
    assert_eq!(
        h.sleep()
            .timers
            .all()
            .iter()
            .filter(|timer| timer.fading)
            .count(),
        MAX_FADES
    );
    // A second scheduler shares the same sleep instance and the same bound.
    let other = Scheduler::new(&h.surface);
    assert!(start_with(&other, h.now, |_| panic!("capacity exceeded")).is_empty());
    drop(queued); // Abandoned closures have not entered the core.
    assert_eq!(h.sleep().fades_in_flight.load(Ordering::Acquire), 0);
    assert!(h.sleep().timers.all().iter().all(|timer| !timer.fading));
}

#[test]
fn an_abandoned_worker_releases_only_its_own_claim() {
    let h = House::new(1, true);
    let scheduler = Scheduler::new(&h.surface);
    let mut queued = Vec::new();
    assert_eq!(
        start_with(&scheduler, h.now, |work| {
            queued.push(work);
            true
        })
        .len(),
        1
    );
    drop(queued);
    let mut retry = Vec::new();
    assert_eq!(
        start_with(&scheduler, h.now, |work| {
            retry.push(work);
            true
        })
        .len(),
        1
    );
    assert_eq!(h.sleep().fades_in_flight.load(Ordering::Acquire), 1);
    drop(retry);
    assert_eq!(h.sleep().fades_in_flight.load(Ordering::Acquire), 0);
}

#[test]
fn a_queued_worker_cannot_pause_a_replacement_timer() {
    let h = House::new(1, false);
    h.play();
    let scheduler = Scheduler::new(&h.surface);
    let mut queued = Vec::new();
    assert_eq!(
        start_with(&scheduler, h.now, |work| {
            queued.push(work);
            true
        })
        .len(),
        1
    );
    let id = h.id("Room0");
    let replacement = h
        .sleep()
        .timers
        .start(
            &h.sim.lan(),
            &h.houses,
            &h.sleep().fader,
            &id,
            Duration::from_mins(20),
            h.now,
        )
        .unwrap();
    let before = h.sim.soap_log().len();
    queued.remove(0)();
    assert_eq!(h.sim.soap_log().len(), before);
    assert_eq!(h.sleep().timers.get(&id), Some(replacement));
    assert_eq!(h.sleep().fades_in_flight.load(Ordering::Acquire), 0);
}

#[test]
fn shutdown_drops_queued_fades_without_late_speaker_commands() {
    let h = House::new(1, true);
    let scheduler = Scheduler::new(&h.surface);
    let mut queued = Vec::new();
    assert_eq!(
        start_with(&scheduler, h.now, |work| {
            queued.push(work);
            true
        })
        .len(),
        1
    );
    scheduler.shut_down(Duration::ZERO);
    let before = h.sim.soap_log().len();
    queued.remove(0)();
    assert_eq!(h.sim.soap_log().len(), before);
    assert_eq!(h.sleep().fades_in_flight.load(Ordering::Acquire), 0);
    assert!(start_with(&scheduler, h.now, |_| panic!("scheduler stopped")).is_empty());
}

#[test]
fn a_lost_pause_acknowledgement_is_never_requeued() {
    let h = House::new(1, true);
    h.play();
    h.lose_pause.store(true, Ordering::Release);
    let scheduler = Scheduler::new(&h.surface);
    assert_eq!(start(&scheduler, h.now).len(), 1);
    assert!(scheduler.wait(Duration::from_secs(5)));
    for _ in 0..10 {
        assert!(start(&scheduler, h.now).is_empty());
    }
    assert_eq!(
        h.sim
            .soap_log()
            .iter()
            .filter(|entry| entry.action == "Pause")
            .count(),
        1
    );
    assert_eq!(
        soap::get_volume(&h.sim.lan(), h.sim.player("Room0").unwrap().ip).unwrap(),
        30
    );
    assert_eq!(h.sleep().fades_in_flight.load(Ordering::Acquire), 0);
}

#[test]
fn interruption_after_failed_spawn_ends_the_timer_without_pausing_music() {
    let h = House::new(1, false);
    h.play();
    let scheduler = Scheduler::new(&h.surface);
    assert!(start_with(&scheduler, h.now, |_| false).is_empty());
    assert!(h.sleep().volume_changed(&h.houses, &h.id("Room0")));
    let host = h.sim.player("Room0").unwrap().ip;
    soap::set_volume(&h.sim.lan(), host, 17).unwrap();
    let before = h.sim.soap_log().len();
    assert_eq!(start(&scheduler, h.now).len(), 1);
    assert!(scheduler.wait(Duration::from_secs(5)));
    let log = h.sim.soap_log();
    assert!(
        log[before..]
            .iter()
            .all(|entry| entry.action == "ConfigureSleepTimer")
    );
    assert_eq!(
        soap::get_transport_info(&h.sim.lan(), host).unwrap().state,
        TransportState::Playing
    );
    assert_eq!(soap::get_volume(&h.sim.lan(), host).unwrap(), 17);
    assert!(h.sleep().timers.all().is_empty());
}
