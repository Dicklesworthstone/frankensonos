//! Native sleep edits and cleanup must remain bound to the intended coordinator.

use chrono::{DateTime, Utc};
use fsonos_core::fade::Fader;
use fsonos_core::sleep::SleepTimers;
use fsonos_core::{CoreError, HouseholdState};
use fsonos_proto::control::{self as soap, SleepTimerInfo};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::cell::{Cell, RefCell};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-07T22:00:00Z")
        .unwrap()
        .to_utc()
}

#[derive(Debug, Clone, Copy)]
enum Change {
    Start,
    Replace,
    Extend,
    Cancel,
}

const CHANGES: [Change; 4] = [
    Change::Start,
    Change::Replace,
    Change::Extend,
    Change::Cancel,
];

impl Change {
    fn run<T: Transport + ?Sized>(
        self,
        t: &T,
        households: &[HouseholdState],
        timers: &SleepTimers,
        fader: &Fader,
        target: &PlayerId,
    ) -> Result<(), CoreError> {
        match self {
            Self::Start | Self::Replace => timers
                .start(t, households, fader, target, Duration::from_mins(10), now())
                .map(drop),
            Self::Extend => timers
                .extend(t, households, fader, target, Duration::from_mins(10), now())
                .map(|timer| assert!(timer.is_some())),
            Self::Cancel => timers
                .cancel(t, households, fader, target)
                .map(|had| assert!(had)),
        }
    }
}

struct Fixture {
    sim: SimHandle,
    houses: Vec<HouseholdState>,
    target: PlayerId,
}

impl Fixture {
    fn new(s2: bool) -> Self {
        let model = if s2 {
            SimModel::One
        } else {
            SimModel::Play5Gen1
        };
        let players = [
            SimPlayerSpec::new("Target", model),
            SimPlayerSpec::new("Peer", model),
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
        for (room, seconds, level) in [("Target", 300, 30), ("Peer", 1800, 20)] {
            let host = sim.player(room).unwrap().ip;
            soap::set_av_transport_uri(
                &lan,
                host,
                "x-rincon-mp3radio://example.invalid/sleep.mp3",
                "",
            )
            .unwrap();
            soap::play(&lan, host).unwrap();
            soap::set_volume(&lan, host, level).unwrap();
            soap::configure_sleep_timer(&lan, host, Some(seconds)).unwrap();
        }
        let host = sim.player("Target").unwrap().ip;
        let target = PlayerId(sim.player("Target").unwrap().uuid.clone());
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
        Self {
            sim,
            houses: vec![house],
            target,
        }
    }

    fn ip(&self, room: &str) -> IpAddr {
        self.sim.player(room).unwrap().ip
    }

    fn stale(&self) -> Vec<HouseholdState> {
        let mut houses = self.houses.clone();
        houses[0]
            .players
            .iter_mut()
            .find(|player| player.id == self.target)
            .unwrap()
            .ip = self.ip("Peer");
        houses
    }

    fn prepare(&self, change: Change, timers: &SleepTimers, fader: &Fader) {
        if !matches!(change, Change::Start) {
            timers
                .start(
                    &self.sim.lan(),
                    &self.houses,
                    fader,
                    &self.target,
                    Duration::ZERO,
                    now(),
                )
                .unwrap();
            assert_eq!(timers.due(now()).len(), 1);
        }
    }

    fn native_timers(&self) -> [SleepTimerInfo; 2] {
        ["Target", "Peer"]
            .map(|room| soap::get_sleep_timer_info(&self.sim.lan(), self.ip(room)).unwrap())
    }

    fn assert_reads_only(&self, from: usize) {
        assert!(
            self.sim.soap_log()[from..]
                .iter()
                .all(|entry| entry.action == "GetZoneGroupState")
        );
    }

    fn assert_timers_unchanged(&self, before: [SleepTimerInfo; 2], observed: Instant) {
        for (old, current) in before.into_iter().zip(self.native_timers()) {
            assert_retained_timer(old, current, observed.elapsed());
        }
    }

    fn assert_audio_unchanged(&self) {
        for (room, level) in [("Target", 30), ("Peer", 20)] {
            let lan = self.sim.lan();
            assert_eq!(soap::get_volume(&lan, self.ip(room)).unwrap(), level);
            assert_eq!(
                soap::get_transport_info(&lan, self.ip(room)).unwrap().state,
                TransportState::Playing
            );
        }
    }
}

fn assert_retained_timer(before: SleepTimerInfo, after: SleepTimerInfo, elapsed: Duration) {
    assert!(before.generation.is_some());
    assert_eq!(
        after.generation, before.generation,
        "no native replacement or clear"
    );
    match (before.remaining_secs, after.remaining_secs) {
        (Some(before), Some(after)) => {
            assert!(after <= before);
            assert!(u64::from(before - after) <= elapsed.as_secs().saturating_add(1));
        }
        (None, None) => {}
        _ => panic!("native timer changed: {before:?} -> {after:?}"),
    }
}

#[test]
fn stale_sleep_bindings_cannot_edit_another_coordinators_native_timer() {
    for s2 in [false, true] {
        for change in CHANGES {
            let f = Fixture::new(s2);
            let timers = SleepTimers::new();
            let fader = Fader::default();
            f.prepare(change, &timers, &fader);
            let original = timers.all();
            let observed = Instant::now();
            let native = f.native_timers();
            let from = f.sim.soap_log().len();
            let error = change
                .run(&f.sim.lan(), &f.stale(), &timers, &fader, &f.target)
                .unwrap_err();
            assert!(
                matches!(&error, CoreError::TargetChanged(_)),
                "{change:?}: {error}"
            );
            f.assert_reads_only(from);
            assert_eq!(timers.all(), original);
            f.assert_timers_unchanged(native, observed);
            f.assert_audio_unchanged();
        }
    }
}

#[test]
fn reused_sleep_addresses_are_refused_before_any_native_write() {
    for s2 in [false, true] {
        for change in CHANGES {
            let mut f = Fixture::new(s2);
            let timers = SleepTimers::new();
            let fader = Fader::default();
            f.prepare(change, &timers, &fader);
            let original = timers.all();
            let observed = Instant::now();
            let native = f.native_timers();
            let IpAddr::V4(old) = f.ip("Target") else {
                panic!("synthetic IPv4 address")
            };
            f.sim.change_address("Target").unwrap();
            f.sim
                .move_to("Peer", old, SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
                .unwrap();
            let from = f.sim.soap_log().len();
            let error = change
                .run(&f.sim.lan(), &f.houses, &timers, &fader, &f.target)
                .unwrap_err();
            assert!(
                matches!(&error, CoreError::TargetChanged(_)),
                "{change:?}: {error}"
            );
            f.assert_reads_only(from);
            assert_eq!(timers.all(), original);
            f.assert_timers_unchanged(native, observed);
            f.assert_audio_unchanged();
        }
    }
}

#[test]
fn unavailable_sleep_endpoints_preserve_the_armed_timer_and_original_error() {
    for s2 in [false, true] {
        for change in CHANGES {
            let mut f = Fixture::new(s2);
            let timers = SleepTimers::new();
            let fader = Fader::default();
            f.prepare(change, &timers, &fader);
            let original = timers.all();
            let observed = Instant::now();
            let native = f.native_timers();
            f.sim.change_address("Target").unwrap();
            let from = f.sim.soap_log().len();
            let error = change
                .run(&f.sim.lan(), &f.houses, &timers, &fader, &f.target)
                .unwrap_err();
            assert!(
                matches!(&error, CoreError::Proto(ProtoError::Network { .. })),
                "{change:?}: {error}"
            );
            f.assert_reads_only(from);
            assert_eq!(timers.all(), original);
            f.assert_timers_unchanged(native, observed);
            f.assert_audio_unchanged();
        }
    }
}

/// The first mandatory check succeeds. The optional preparation then observes
/// the same address after another real coordinator has acquired it.
struct ReassignedDuringPreparation<'a> {
    sim: RefCell<&'a mut SimHandle>,
    inner: SimLan,
    topology_reads: Cell<usize>,
    writes: Cell<usize>,
    moved: Cell<bool>,
}

impl Transport for ReassignedDuringPreparation<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        if name == "GetZoneGroupState" {
            let reads = self.topology_reads.get() + 1;
            self.topology_reads.set(reads);
            if reads == 2 {
                let IpAddr::V4(old) = host else {
                    panic!("synthetic IPv4 address")
                };
                let mut sim = self.sim.borrow_mut();
                sim.change_address("Target").unwrap();
                sim.move_to("Peer", old, SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
                    .unwrap();
                self.moved.set(true);
            }
        }
        if name == "ConfigureSleepTimer" {
            self.writes.set(self.writes.get() + 1);
        }
        self.inner.soap_post(host, path, action, body)
    }
}

#[test]
fn an_endpoint_changed_during_sleep_preparation_is_refused_before_dispatch() {
    for s2 in [false, true] {
        for change in CHANGES {
            let mut f = Fixture::new(s2);
            let timers = SleepTimers::new();
            let fader = Fader::default();
            f.prepare(change, &timers, &fader);
            let original = timers.all();
            let observed = Instant::now();
            let native = f.native_timers();
            let from = f.sim.soap_log().len();
            let inner = f.sim.lan();
            let replies = ReassignedDuringPreparation {
                sim: RefCell::new(&mut f.sim),
                inner,
                topology_reads: Cell::new(0),
                writes: Cell::new(0),
                moved: Cell::new(false),
            };
            let error = change
                .run(&replies, &f.houses, &timers, &fader, &f.target)
                .unwrap_err();
            assert!(
                matches!(&error, CoreError::TargetChanged(_)),
                "{change:?}: {error}"
            );
            assert!(replies.moved.get());
            assert_eq!(replies.writes.get(), 0);
            drop(replies);
            f.assert_reads_only(from);
            assert_eq!(timers.all(), original);
            f.assert_timers_unchanged(native, observed);
            f.assert_audio_unchanged();
        }
    }
}

struct MissingGeneration<'a> {
    inner: &'a SimLan,
    lose_reply: bool,
    writes: AtomicUsize,
}

impl Transport for MissingGeneration<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        if name == "ConfigureSleepTimer" {
            self.writes.fetch_add(1, Ordering::SeqCst);
        }
        let reply = self.inner.soap_post(host, path, action, body)?;
        if name == "ConfigureSleepTimer" && self.lose_reply {
            return Err(ProtoError::Network {
                target: host.to_string(),
                detail: "injected missing reply without timer-generation evidence".into(),
            });
        }
        if name == "GetRemainingSleepTimerDuration" {
            return Ok(reply.replace("CurrentSleepTimerGeneration", "AbsentGeneration"));
        }
        Ok(reply)
    }
}

#[test]
fn missing_generation_evidence_allows_acknowledged_changes_only_at_verified_endpoints() {
    for s2 in [false, true] {
        for change in CHANGES {
            for lose_reply in [false, true] {
                let f = Fixture::new(s2);
                let timers = SleepTimers::new();
                let fader = Fader::default();
                f.prepare(change, &timers, &fader);
                let observed = Instant::now();
                let before = f.native_timers();
                let lan = f.sim.lan();
                let replies = MissingGeneration {
                    inner: &lan,
                    lose_reply,
                    writes: AtomicUsize::new(0),
                };
                let result = change.run(&replies, &f.houses, &timers, &fader, &f.target);
                if lose_reply {
                    assert!(matches!(result, Err(CoreError::Uncertain(_))));
                    assert!(timers.all().is_empty());
                } else {
                    result.unwrap();
                    assert_eq!(
                        timers.all().len(),
                        usize::from(!matches!(change, Change::Cancel))
                    );
                }
                assert_eq!(replies.writes.load(Ordering::SeqCst), 1);
                let after = f.native_timers();
                assert_eq!(
                    after[0].generation,
                    before[0]
                        .generation
                        .and_then(|generation| generation.checked_add(1))
                );
                if matches!(change, Change::Cancel) {
                    assert!(matches!(after[0].remaining_secs, None | Some(0)));
                } else {
                    let remaining = after[0].remaining_secs.unwrap();
                    assert!(remaining <= 900);
                    assert!(
                        u64::from(900 - remaining)
                            <= observed.elapsed().as_secs().saturating_add(1)
                    );
                }
                assert_retained_timer(before[1], after[1], observed.elapsed());
                f.assert_audio_unchanged();
            }
        }
    }
}

#[test]
fn announcement_rearm_refusal_cannot_clear_a_foreign_backstop() {
    for s2 in [false, true] {
        let f = Fixture::new(s2);
        let timers = SleepTimers::new();
        let fader = Fader::default();
        f.prepare(Change::Replace, &timers, &fader);
        let receipt = timers
            .interrupt_for_announcement(&fader, &f.target)
            .unwrap();
        let original = timers.all();
        let observed = Instant::now();
        let native = f.native_timers();
        let from = f.sim.soap_log().len();
        let error = timers
            .resume_after_announcement(&f.sim.lan(), &f.stale(), &fader, &receipt, now())
            .unwrap_err();
        assert!(matches!(error, CoreError::TargetChanged(_)));
        f.assert_reads_only(from);
        assert_eq!(timers.all(), original);
        f.assert_timers_unchanged(native, observed);
        f.assert_audio_unchanged();
        let from = f.sim.soap_log().len();
        assert!(
            timers
                .resume_after_announcement(&f.sim.lan(), &f.houses, &fader, &receipt, now())
                .unwrap()
                .is_none()
        );
        assert_eq!(
            f.sim.soap_log().len(),
            from,
            "the failed receipt stays retired"
        );
    }
}
