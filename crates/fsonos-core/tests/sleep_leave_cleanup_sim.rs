//! A confirmed leave can use a household witness for stale self-membership,
//! while every native clear remains bound to the original physical endpoint.

use chrono::{DateTime, Utc};
use fsonos_core::fade::Fader;
use fsonos_core::sleep::{SleepOutcome, SleepTimers};
use fsonos_core::{CoreError, HouseholdState};
use fsonos_proto::control::{self as soap, SleepTimerInfo};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::cell::{Cell, RefCell};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-07T22:00:00Z")
        .unwrap()
        .to_utc()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fault {
    None,
    FirstWitnessUnavailable,
    TargetMoved,
    TargetMovedWithFreshSelf,
    WitnessMoved,
    SelfUnavailable,
    SelfMalformed,
    LostClear,
}

struct Fixture {
    sim: SimHandle,
    lan: SimLan,
    houses: Vec<HouseholdState>,
    target: PlayerId,
    peer: PlayerId,
}

impl Fixture {
    fn new(s2: bool, foreign_other: bool) -> Self {
        let model = if s2 {
            SimModel::One
        } else {
            SimModel::Play5Gen1
        };
        let mut players = vec![
            SimPlayerSpec::new("Target", model),
            SimPlayerSpec::new("Peer", model),
        ];
        if !foreign_other {
            players.push(SimPlayerSpec::new("Other", model));
        }
        let builder = SimHousehold::builder();
        let builder = if s2 {
            builder.s2(players)
        } else {
            builder.s1(players)
        };
        let builder = if foreign_other {
            if s2 {
                builder.s1([SimPlayerSpec::new("Other", SimModel::Play5Gen1)])
            } else {
                builder.s2([SimPlayerSpec::new("Other", SimModel::One)])
            }
        } else {
            builder
        };
        let sim = builder.spawn().unwrap();
        let lan = sim.lan();
        for (room, seconds, level) in [("Target", 300, 30), ("Peer", 1800, 20), ("Other", 3600, 15)]
        {
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
        let target = PlayerId(sim.player("Target").unwrap().uuid.clone());
        let peer = PlayerId(sim.player("Peer").unwrap().uuid.clone());
        let mut house = HouseholdState::default();
        house
            .apply_topology(&get_zone_group_state(&lan, sim.player("Target").unwrap().ip).unwrap());
        let mut houses = vec![house];
        if foreign_other {
            let mut foreign = HouseholdState::default();
            foreign.apply_topology(
                &get_zone_group_state(&lan, sim.player("Other").unwrap().ip).unwrap(),
            );
            houses.push(foreign);
        }
        Self {
            sim,
            lan,
            houses,
            target,
            peer,
        }
    }

    fn ip(&self, room: &str) -> IpAddr {
        self.sim.player(room).unwrap().ip
    }

    fn timers(&self) -> [SleepTimerInfo; 3] {
        ["Target", "Peer", "Other"]
            .map(|room| soap::get_sleep_timer_info(&self.lan, self.ip(room)).unwrap())
    }

    fn join_target(&mut self) {
        soap::join_group(&self.lan, self.ip("Target"), &self.peer).unwrap();
        self.houses[0].apply_topology(&get_zone_group_state(&self.lan, self.ip("Peer")).unwrap());
        // The leave path's preferred witness must also be the first cleanup
        // candidate, so its injected failure exercises the following peer.
        self.houses[0]
            .players
            .sort_by_key(|player| player.id != self.peer);
    }
}

/// Keep the acting player's first (joined) topology indefinitely. The leave
/// itself is real; Peer first confirms it. Faults begin only when cleanup reads
/// self-topology afterward, after the old fade has restored its volume.
struct StaleMembership<'a> {
    sim: RefCell<&'a mut SimHandle>,
    inner: &'a SimLan,
    target: PlayerId,
    target_host: IpAddr,
    peer_host: IpAddr,
    fault: Fault,
    stale: RefCell<Option<String>>,
    left: Cell<bool>,
    confirmed: Cell<bool>,
    cleanup: Cell<bool>,
    clears: Cell<usize>,
    reads_after_clear: Cell<usize>,
    witnesses: RefCell<Vec<IpAddr>>,
}

impl StaleMembership<'_> {
    fn unavailable(host: IpAddr, detail: &str) -> ProtoError {
        ProtoError::Network {
            target: host.to_string(),
            detail: detail.into(),
        }
    }

    fn begin_cleanup(&self) {
        let room = match self.fault {
            Fault::TargetMoved | Fault::TargetMovedWithFreshSelf => "Target",
            Fault::WitnessMoved => "Peer",
            _ => return,
        };
        let mut sim = self.sim.borrow_mut();
        let IpAddr::V4(old) = sim.player(room).unwrap().ip else {
            panic!("synthetic IPv4 address")
        };
        sim.change_address(room).unwrap();
        sim.move_to("Other", old, SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .unwrap();
    }
}

impl Transport for StaleMembership<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        if name == "GetZoneGroupState" {
            if self.clears.get() > 0 {
                self.reads_after_clear.set(self.reads_after_clear.get() + 1);
            }
            if host == self.target_host {
                if self.confirmed.get() && !self.cleanup.replace(true) {
                    self.begin_cleanup();
                }
                if self.cleanup.get() {
                    match self.fault {
                        Fault::SelfUnavailable => {
                            return Err(Self::unavailable(host, "self topology unavailable"));
                        }
                        Fault::SelfMalformed => return Ok("invalid self topology".into()),
                        Fault::TargetMovedWithFreshSelf => {
                            return self.inner.soap_post(host, path, action, body);
                        }
                        _ => {}
                    }
                }
                if let Some(stale) = self.stale.borrow().as_ref() {
                    return Ok(stale.clone());
                }
                let reply = self.inner.soap_post(host, path, action, body)?;
                *self.stale.borrow_mut() = Some(reply.clone());
                return Ok(reply);
            }
            if self.cleanup.get() {
                self.witnesses.borrow_mut().push(host);
                if self.fault == Fault::FirstWitnessUnavailable && host == self.peer_host {
                    return Err(Self::unavailable(host, "first witness unavailable"));
                }
            }
            let reply = self.inner.soap_post(host, path, action, body)?;
            if self.left.get() && !self.cleanup.get() {
                self.confirmed.set(true);
            }
            return Ok(reply);
        }
        if name == "ConfigureSleepTimer" {
            self.clears.set(self.clears.get() + 1);
        }
        let reply = self.inner.soap_post(host, path, action, body)?;
        if name == "SetAVTransportURI" && body.contains(&format!("x-rincon:{}", self.target.0)) {
            self.left.set(true);
        }
        if name == "ConfigureSleepTimer" && self.fault == Fault::LostClear {
            return Err(Self::unavailable(host, "native clear reply lost"));
        }
        Ok(reply)
    }
}

fn retained(before: SleepTimerInfo, after: SleepTimerInfo, elapsed: Duration) {
    assert!(before.generation.is_some());
    assert_eq!(after.generation, before.generation);
    let before = before.remaining_secs.unwrap();
    let after = after.remaining_secs.unwrap();
    assert!(after <= before);
    assert!(u64::from(before - after) <= elapsed.as_secs().saturating_add(1));
}

fn check_cleanup(s2: bool, foreign_other: bool, fault: Fault) {
    let mut f = Fixture::new(s2, foreign_other);
    let timers = SleepTimers::new();
    let fader = Fader::default();
    timers
        .start(&f.lan, &f.houses, &fader, &f.target, Duration::ZERO, now())
        .unwrap();
    assert_eq!(timers.due(now()).len(), 1);
    let observed = Instant::now();
    let before = f.timers();
    f.join_target();
    let target_host = f.ip("Target");
    let peer_host = f.ip("Peer");
    let other_host = f.ip("Other");
    let wire = StaleMembership {
        sim: RefCell::new(&mut f.sim),
        inner: &f.lan,
        target: f.target.clone(),
        target_host,
        peer_host,
        fault,
        stale: RefCell::new(None),
        left: Cell::new(false),
        confirmed: Cell::new(false),
        cleanup: Cell::new(false),
        clears: Cell::new(0),
        reads_after_clear: Cell::new(0),
        witnesses: RefCell::new(Vec::new()),
    };
    let outcome = timers.run(&wire, &f.houses, &fader, &f.target);
    assert!(wire.left.get() && wire.confirmed.get() && wire.cleanup.get());
    let cleared = fault == Fault::None
        || fault == Fault::LostClear
        || (fault == Fault::FirstWitnessUnavailable && !foreign_other);
    assert_eq!(wire.clears.get(), usize::from(cleared));
    assert_eq!(
        wire.reads_after_clear.get(),
        0,
        "a native clear is never replayed or reauthorized"
    );
    match fault {
        Fault::SelfUnavailable => assert!(
            matches!(&outcome, Err(CoreError::Proto(ProtoError::Network { detail, .. })) if detail == "self topology unavailable")
        ),
        Fault::SelfMalformed => assert!(matches!(
            outcome,
            Err(CoreError::Proto(ProtoError::Malformed(_)))
        )),
        Fault::LostClear => assert!(
            matches!(&outcome, Err(CoreError::Proto(ProtoError::Network { detail, .. })) if detail == "native clear reply lost")
        ),
        _ if cleared => assert_eq!(outcome.unwrap(), SleepOutcome::Paused),
        _ => assert!(matches!(outcome, Err(CoreError::TargetChanged(_)))),
    }
    if matches!(
        fault,
        Fault::SelfUnavailable | Fault::SelfMalformed | Fault::TargetMovedWithFreshSelf
    ) {
        assert!(
            wire.witnesses.borrow().is_empty(),
            "strict self-evidence failures cannot be overridden"
        );
    }
    if fault == Fault::FirstWitnessUnavailable {
        let witnesses = wire.witnesses.borrow();
        assert_eq!(witnesses.first(), Some(&peer_host));
        assert_eq!(witnesses.contains(&other_host), !foreign_other);
    }
    drop(wire);
    assert!(timers.all().is_empty());
    let after = f.timers();
    if cleared {
        assert_eq!(
            after[0].generation,
            before[0]
                .generation
                .and_then(|generation| generation.checked_add(1))
        );
        assert!(matches!(after[0].remaining_secs, None | Some(0)));
    } else {
        retained(before[0], after[0], observed.elapsed());
    }
    retained(before[1], after[1], observed.elapsed());
    retained(before[2], after[2], observed.elapsed());
    for (room, level) in [("Target", 30), ("Peer", 20), ("Other", 15)] {
        assert_eq!(soap::get_volume(&f.lan, f.ip(room)).unwrap(), level);
    }
    for room in ["Peer", "Other"] {
        assert_eq!(
            soap::get_transport_info(&f.lan, f.ip(room)).unwrap().state,
            TransportState::Playing
        );
    }
}

#[test]
fn a_joined_sleep_clears_its_backstop_using_a_fresh_household_witness() {
    for s2 in [false, true] {
        for fault in [Fault::None, Fault::FirstWitnessUnavailable] {
            check_cleanup(s2, false, fault);
        }
    }
}

#[test]
fn post_leave_sleep_cleanup_rejects_moved_targets_and_witnesses() {
    for s2 in [false, true] {
        for foreign_other in [false, true] {
            for fault in [
                Fault::TargetMoved,
                Fault::TargetMovedWithFreshSelf,
                Fault::WitnessMoved,
            ] {
                check_cleanup(s2, foreign_other, fault);
            }
        }
    }
}

#[test]
fn post_leave_cleanup_does_not_search_another_household() {
    for s2 in [false, true] {
        check_cleanup(s2, true, Fault::FirstWitnessUnavailable);
    }
}

#[test]
fn post_leave_cleanup_keeps_unreadable_self_errors_and_never_replays_a_clear() {
    for s2 in [false, true] {
        for fault in [
            Fault::SelfUnavailable,
            Fault::SelfMalformed,
            Fault::LostClear,
        ] {
            check_cleanup(s2, false, fault);
        }
    }
}
