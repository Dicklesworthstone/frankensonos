//! Fades and restoration follow physical players even when cached addresses
//! are reassigned. Every fixture uses real S1 or S2 loopback endpoints.

use super::*;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use std::cell::{Cell, RefCell};
use std::net::{Ipv4Addr, SocketAddr};

struct Fixture {
    sim: SimHandle,
    houses: Vec<HouseholdState>,
    target: PlayerId,
    host: IpAddr,
    peer: IpAddr,
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
        let host = sim.player("Target").unwrap().ip;
        let peer = sim.player("Peer").unwrap().ip;
        let target = PlayerId(sim.player("Target").unwrap().uuid.clone());
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
        soap::set_volume(&lan, host, 40).unwrap();
        soap::set_volume(&lan, peer, 20).unwrap();
        Self {
            sim,
            houses: vec![house],
            target,
            host,
            peer,
        }
    }

    fn stale(&self) -> Vec<HouseholdState> {
        let mut houses = self.houses.clone();
        houses[0]
            .players
            .iter_mut()
            .find(|player| player.id == self.target)
            .unwrap()
            .ip = self.peer;
        houses
    }

    fn join_peer(&mut self) {
        let lan = self.sim.lan();
        let peer = PlayerId(self.sim.player("Peer").unwrap().uuid.clone());
        soap::join_group(&lan, self.host, &peer).unwrap();
        self.houses[0].apply_topology(&get_zone_group_state(&lan, self.host).unwrap());
    }

    fn play(&self) {
        crate::control::play_uri(
            &self.sim.lan(),
            &self.houses,
            &self.target,
            "x-rincon-mp3radio://stream.example.invalid/fade.mp3",
            "",
        )
        .unwrap();
    }
}

#[derive(Clone, Copy, Debug)]
enum Operation {
    Manual,
    Fade,
    Each,
    Claimed,
    Sleep,
    Device,
    Restore,
    Pause,
}

const OPERATIONS: [Operation; 8] = [
    Operation::Manual,
    Operation::Fade,
    Operation::Each,
    Operation::Claimed,
    Operation::Sleep,
    Operation::Device,
    Operation::Restore,
    Operation::Pause,
];

impl Operation {
    fn run(
        self,
        fader: &Fader,
        t: &impl Transport,
        houses: &[HouseholdState],
        target: &PlayerId,
    ) -> Result<(), CoreError> {
        match self {
            Self::Manual => fader.set_volume(t, houses, target, 30),
            Self::Fade => fader.fade(t, houses, target, 30, Duration::ZERO).map(drop),
            Self::Each => fader
                .fade_each(t, houses, &[(target.clone(), 30)], Duration::ZERO)
                .map(drop),
            Self::Claimed => {
                let claimed = fader.claim(std::slice::from_ref(target));
                fader
                    .fade_group_claimed_until(t, houses, &claimed, 30, Instant::now())
                    .map(drop)
            }
            Self::Sleep => {
                let claimed = fader.claim(std::slice::from_ref(target));
                fader
                    .sleep_fade_until(t, houses, &claimed, Instant::now(), |_, _, _| Ok(()))
                    .map(drop)
            }
            Self::Device => fader.ramp_at_device_speed(t, houses, target, 30).map(drop),
            Self::Restore => {
                let intent = fader.supersede(target);
                fader.restore(t, houses, &[(target.clone(), intent, 30)])
            }
            Self::Pause => fader
                .fade_out_and_pause(t, houses, target, Duration::ZERO)
                .map(drop),
        }
    }

    fn level(self) -> u8 {
        match self {
            Self::Sleep => 0,
            Self::Pause => 40,
            _ => 30,
        }
    }
}

#[test]
fn stale_fade_endpoints_cannot_change_another_rooms_volume_or_playback() {
    for s2 in [false, true] {
        let f = Fixture::new(s2);
        let lan = f.sim.lan();
        for operation in OPERATIONS {
            let before = f.sim.soap_log().len();
            let error = operation
                .run(&Fader::default(), &lan, &f.stale(), &f.target)
                .unwrap_err();
            assert!(
                matches!(error, CoreError::TargetChanged(_)),
                "{operation:?}, S2={s2}: {error}"
            );
            let actions: Vec<_> = f.sim.soap_log()[before..]
                .iter()
                .map(|entry| entry.action.clone())
                .collect();
            assert_eq!(actions, ["GetZoneGroupState"], "{operation:?}");
            assert_eq!(soap::get_volume(&lan, f.host).unwrap(), 40);
            assert_eq!(soap::get_volume(&lan, f.peer).unwrap(), 20);
        }
    }
}

#[test]
fn admitted_fades_ramps_and_restoration_still_change_the_requested_player() {
    for s2 in [false, true] {
        for operation in OPERATIONS {
            let f = Fixture::new(s2);
            let lan = f.sim.lan();
            operation
                .run(&Fader::default(), &lan, &f.houses, &f.target)
                .unwrap();
            assert_eq!(
                soap::get_volume(&lan, f.host).unwrap(),
                operation.level(),
                "{operation:?}, S2={s2}"
            );
            assert_eq!(soap::get_volume(&lan, f.peer).unwrap(), 20);
        }
    }
}

#[test]
fn a_group_member_remains_a_valid_room_volume_target() {
    for s2 in [false, true] {
        for operation in OPERATIONS {
            if matches!(operation, Operation::Pause) {
                continue;
            }
            let mut f = Fixture::new(s2);
            f.join_peer();
            let lan = f.sim.lan();
            operation
                .run(&Fader::default(), &lan, &f.houses, &f.target)
                .unwrap();
            assert_eq!(soap::get_volume(&lan, f.host).unwrap(), operation.level());
            assert_eq!(soap::get_volume(&lan, f.peer).unwrap(), 20);
        }
    }
}

#[derive(Clone, Copy)]
enum ChangeAt {
    VolumeRead,
    VolumeWrite,
}

struct Reassigned {
    sim: RefCell<SimHandle>,
    inner: SimLan,
    host: IpAddr,
    change_at: ChangeAt,
    changed: Cell<bool>,
    writes: RefCell<Vec<String>>,
}

impl Reassigned {
    fn new(sim: SimHandle, host: IpAddr, change_at: ChangeAt) -> Self {
        Self {
            inner: sim.lan(),
            sim: RefCell::new(sim),
            host,
            change_at,
            changed: Cell::new(false),
            writes: RefCell::new(Vec::new()),
        }
    }

    fn assert_levels(&self, target: u8, peer: u8) {
        let sim = self.sim.borrow();
        assert_eq!(
            soap::get_volume(&self.inner, sim.player("Target").unwrap().ip).unwrap(),
            target
        );
        assert_eq!(
            soap::get_volume(&self.inner, sim.player("Peer").unwrap().ip).unwrap(),
            peer
        );
    }
}

impl Transport for Reassigned {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action
            .trim_matches('"')
            .rsplit('#')
            .next()
            .unwrap_or(action);
        if !name.starts_with("Get") && !matches!(name, "Browse" | "Search") {
            self.writes.borrow_mut().push(name.into());
        }
        let reply = self.inner.soap_post(host, path, action, body)?;
        let trigger = match self.change_at {
            ChangeAt::VolumeRead => "GetVolume",
            ChangeAt::VolumeWrite => "SetVolume",
        };
        if host == self.host && name == trigger && !self.changed.replace(true) {
            let IpAddr::V4(old) = host else {
                panic!("the simulator uses IPv4");
            };
            let mut sim = self.sim.borrow_mut();
            sim.change_address("Target").unwrap();
            sim.move_to("Peer", old, SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
                .unwrap();
        }
        Ok(reply)
    }
}

#[test]
fn address_reuse_after_a_volume_read_is_rejected_before_the_first_fade_write() {
    for s2 in [false, true] {
        for operation in [
            Operation::Fade,
            Operation::Each,
            Operation::Claimed,
            Operation::Sleep,
            Operation::Pause,
        ] {
            let f = Fixture::new(s2);
            let t = Reassigned::new(f.sim, f.host, ChangeAt::VolumeRead);
            let error = operation
                .run(&Fader::default(), &t, &f.houses, &f.target)
                .unwrap_err();
            assert!(t.changed.get(), "{operation:?}, S2={s2}");
            assert!(matches!(error, CoreError::TargetChanged(_)), "{error}");
            assert!(t.writes.borrow().is_empty(), "{:?}", t.writes.borrow());
            t.assert_levels(40, 20);
        }
    }
}

#[test]
fn a_reassigned_address_cannot_receive_the_old_players_restoration() {
    for s2 in [false, true] {
        let f = Fixture::new(s2);
        f.play();
        let t = Reassigned::new(f.sim, f.host, ChangeAt::VolumeWrite);
        let error = Operation::Pause
            .run(&Fader::default(), &t, &f.houses, &f.target)
            .unwrap_err();
        assert!(t.changed.get());
        assert!(matches!(error, CoreError::TargetChanged(_)), "{error}");
        assert_eq!(*t.writes.borrow(), ["SetVolume"]);
        t.assert_levels(0, 20);
    }
}
