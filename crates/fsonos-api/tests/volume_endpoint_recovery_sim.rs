//! Volume and mute follow physical rooms through address changes. Policy
//! reads and group snapshots cannot make a reused address their new target.

use fsonos_api::heal::execute_healing;
use fsonos_api::plan::{VolumeScope, plan_mute};
use fsonos_api::surface::{Surface, Survey};
use fsonos_api::{Command, ErrorCode, Guard, MuteRequest, NoteCode, VolumeChange};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::{CoreError, HouseholdState};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec, SoapLogEntry};
use fsonos_types::PlayerId;
use std::cell::Cell;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

fn survey<T: Transport + ?Sized>(
    transport: &T,
    peer: IpAddr,
) -> Result<Vec<HouseholdState>, CoreError> {
    let mut house = HouseholdState::default();
    house.apply_topology(&get_zone_group_state(transport, peer)?);
    Ok(vec![house])
}

fn snapshot(sim: &SimHandle) -> Vec<HouseholdState> {
    survey(&sim.lan(), sim.player("Peer").unwrap().ip).unwrap()
}

struct Rig {
    sim: SimHandle,
    houses: Vec<HouseholdState>,
    target: PlayerId,
    leader: PlayerId,
}

impl Rig {
    fn new(s2: bool) -> Self {
        let model = if s2 {
            SimModel::One
        } else {
            SimModel::Play5Gen1
        };
        let players = ["Target", "Leader", "Peer", "Other"]
            .map(|room| SimPlayerSpec::new(room, model));
        let builder = SimHousehold::builder();
        let sim = if s2 {
            builder.s2(players)
        } else {
            builder.s1(players)
        }
        .spawn()
        .unwrap();
        let target = PlayerId(sim.player("Target").unwrap().uuid.clone());
        let leader = PlayerId(sim.player("Leader").unwrap().uuid.clone());
        soap::join_group(&sim.lan(), sim.player("Target").unwrap().ip, &leader).unwrap();
        for (room, level) in [("Target", 10), ("Leader", 10), ("Other", 60)] {
            soap::set_volume(&sim.lan(), sim.player(room).unwrap().ip, level).unwrap();
        }
        let houses = snapshot(&sim);
        assert_eq!(houses[0].coordinator_of(&target), Some(&leader));
        Self {
            sim,
            houses,
            target,
            leader,
        }
    }

    fn host(&self, room: &str) -> IpAddr {
        self.sim.player(room).unwrap().ip
    }

    fn moved(&mut self, room: &str, reused: bool) -> IpAddr {
        let old = self.host(room);
        let new = self.sim.change_address(room).unwrap();
        if reused {
            let IpAddr::V4(address) = old else {
                panic!("the simulator uses IPv4");
            };
            self.sim
                .move_to("Other", address, SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
                .unwrap();
        }
        new
    }

    fn volume(&self, scope: VolumeScope, change: VolumeChange) -> Command {
        Command::Volume {
            member: self.target.clone(),
            target: match scope {
                VolumeScope::Room => self.target.clone(),
                VolumeScope::Group => self.leader.clone(),
            },
            scope,
            change,
        }
    }
}

fn guard(client: &Client, policy: Policy) -> Guard<'_> {
    Guard {
        policy: Arc::new(policy),
        client,
        clock: &SystemClock,
    }
}

fn changes(log: &[SoapLogEntry]) -> Vec<(&str, &str)> {
    log.iter()
        .filter(|entry| !entry.action.starts_with("Get") && entry.action != "Browse")
        .map(|entry| (entry.room.as_str(), entry.action.as_str()))
        .collect()
}

#[test]
fn room_controls_recover_the_noncoordinator_uuid_without_changing_the_new_owner() {
    for s2 in [false, true] {
        for reused in [false, true] {
            for write in ["SetVolume", "SetRelativeVolume", "SetMute"] {
                let mut rig = Rig::new(s2);
                let (command, expected) = match write {
                    "SetVolume" => (rig.volume(VolumeScope::Room, VolumeChange::Set(25)), 25),
                    "SetRelativeVolume" => {
                        (rig.volume(VolumeScope::Room, VolumeChange::Adjust(5)), 15)
                    }
                    _ => (
                        Command::Mute {
                            target: rig.target.clone(),
                            mute: true,
                        },
                        10,
                    ),
                };
                let new = rig.moved("Target", reused);
                let surveys = Cell::new(0);
                let mark = rig.sim.soap_log().len();
                let outcome = execute_healing(
                    &rig.sim.lan(),
                    &rig.houses,
                    &guard(&Client::Cli, Policy::default()),
                    &command,
                    || {
                        surveys.set(surveys.get() + 1);
                        Some(snapshot(&rig.sim))
                    },
                )
                .unwrap();
                assert_eq!(surveys.get(), 1, "{write}, reused={reused}");
                assert!(outcome.notes.iter().any(|note| note.code == NoteCode::Healed));
                let log = rig.sim.soap_log();
                assert_eq!(changes(&log[mark..]), [("Target", write)], "{log:?}");
                assert_eq!(soap::get_volume(&rig.sim.lan(), new).unwrap(), expected);
                assert_eq!(soap::get_mute(&rig.sim.lan(), new).unwrap(), write == "SetMute");
                for (room, level) in [("Leader", 10), ("Other", 60)] {
                    assert_eq!(soap::get_volume(&rig.sim.lan(), rig.host(room)).unwrap(), level);
                    assert!(!soap::get_mute(&rig.sim.lan(), rig.host(room)).unwrap());
                }
            }
        }
    }
}

#[test]
fn group_controls_guard_both_the_snapshot_and_the_final_write_after_recovery() {
    for s2 in [false, true] {
        for reused in [false, true] {
            for (change, write, expected) in [
                (VolumeChange::Set(25), "SetGroupVolume", 25),
                (VolumeChange::Adjust(5), "SetRelativeGroupVolume", 15),
            ] {
                let mut rig = Rig::new(s2);
                let command = rig.volume(VolumeScope::Group, change);
                rig.moved("Leader", reused);
                let surveys = Cell::new(0);
                let mark = rig.sim.soap_log().len();
                let outcome = execute_healing(
                    &rig.sim.lan(),
                    &rig.houses,
                    &guard(&Client::Cli, Policy::default()),
                    &command,
                    || {
                        surveys.set(surveys.get() + 1);
                        Some(snapshot(&rig.sim))
                    },
                )
                .unwrap();
                assert_eq!(surveys.get(), 1);
                assert_eq!(outcome.volume, Some(expected));
                let log = rig.sim.soap_log();
                assert_eq!(
                    changes(&log[mark..]),
                    [("Leader", "SnapshotGroupVolume"), ("Leader", write)],
                    "{log:?}"
                );
                let leader_actions: Vec<_> = log[mark..]
                    .iter()
                    .filter(|entry| entry.room == "Leader")
                    .map(|entry| entry.action.as_str())
                    .collect();
                assert_eq!(
                    leader_actions,
                    ["GetZoneGroupState", "SnapshotGroupVolume", "GetZoneGroupState", write]
                );
                for room in ["Target", "Leader"] {
                    assert_eq!(
                        soap::get_volume(&rig.sim.lan(), rig.host(room)).unwrap(),
                        expected
                    );
                }
                assert_eq!(soap::get_volume(&rig.sim.lan(), rig.host("Other")).unwrap(), 60);
            }
        }
    }
}

#[test]
fn capped_volume_reads_the_requested_room_before_spending_its_step_allowance() {
    for s2 in [false, true] {
        let mut rig = Rig::new(s2);
        let command = rig.volume(VolumeScope::Room, VolumeChange::Set(60));
        let new = rig.moved("Target", true);
        let policy = Policy::from_toml("[defaults]\nmax_volume = 80\nmax_step = 5\n").unwrap();
        let mark = rig.sim.soap_log().len();
        let outcome = execute_healing(
            &rig.sim.lan(),
            &rig.houses,
            &guard(&Client::McpStdio, policy),
            &command,
            || Some(snapshot(&rig.sim)),
        )
        .unwrap();
        assert_eq!(outcome.volume, Some(15));
        assert!(
            outcome
                .notes
                .iter()
                .any(|note| note.code == NoteCode::VolumeClamped)
        );
        let log = rig.sim.soap_log();
        assert!(
            log[mark..]
                .iter()
                .all(|entry| entry.room != "Other" || entry.action == "GetZoneGroupState"),
            "the reused address cannot supply the policy's starting level: {log:?}"
        );
        assert_eq!(changes(&log[mark..]), [("Target", "SetVolume")]);
        assert_eq!(soap::get_volume(&rig.sim.lan(), new).unwrap(), 15);
        assert_eq!(soap::get_volume(&rig.sim.lan(), rig.host("Other")).unwrap(), 60);
    }
}

#[test]
fn a_capped_unmute_lowers_the_original_room_before_unmuting_it() {
    for s2 in [false, true] {
        let mut rig = Rig::new(s2);
        soap::set_volume(&rig.sim.lan(), rig.host("Target"), 80).unwrap();
        soap::set_mute(&rig.sim.lan(), rig.host("Target"), true).unwrap();
        soap::set_volume(&rig.sim.lan(), rig.host("Other"), 90).unwrap();
        let peer = rig.host("Peer");
        let surveys = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&surveys);
        let read: Survey = Box::new(move |transport| {
            counted.fetch_add(1, Ordering::SeqCst);
            survey(transport, peer)
        });
        let policy = Policy::from_toml("[defaults]\nmax_volume = 30\n").unwrap();
        let surface = Surface::new(
            Box::new(rig.sim.lan()),
            read,
            policy,
            Box::new(SystemClock),
        );
        surface.households().unwrap();
        let new = rig.moved("Target", true);
        let mark = rig.sim.soap_log().len();
        let request = MuteRequest {
            zone: "Target".into(),
            mute: false,
        };
        let outcome = surface
            .control(&Client::McpStdio, "mute", |rooms| plan_mute(rooms, &request))
            .unwrap();
        assert_eq!(surveys.load(Ordering::SeqCst), 2);
        assert!(
            outcome
                .notes
                .iter()
                .any(|note| note.code == NoteCode::VolumeClamped)
        );
        let log = rig.sim.soap_log();
        assert_eq!(
            changes(&log[mark..]),
            [("Target", "SetVolume"), ("Target", "SetMute")],
            "{log:?}"
        );
        assert!(
            log[mark..]
                .iter()
                .all(|entry| entry.room != "Other" || entry.action == "GetZoneGroupState")
        );
        assert_eq!(soap::get_volume(&rig.sim.lan(), new).unwrap(), 30);
        assert!(!soap::get_mute(&rig.sim.lan(), new).unwrap());
        assert_eq!(soap::get_volume(&rig.sim.lan(), rig.host("Other")).unwrap(), 90);
        assert_eq!(soap::get_volume(&rig.sim.lan(), rig.host("Leader")).unwrap(), 10);
    }
}

/// The device applies the step; only its acknowledgement is discarded.
struct LostStep {
    lan: SimLan,
    action: &'static str,
    writes: Cell<usize>,
}

impl Transport for LostStep {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        header: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let reply = self.lan.soap_post(host, path, header, body)?;
        let action = header.trim_matches('"').rsplit('#').next().unwrap();
        if action != self.action {
            return Ok(reply);
        }
        fsonos_proto::soap::parse_response(&reply, action)?;
        self.writes.set(self.writes.get() + 1);
        Err(ProtoError::Network {
            target: "simulator".into(),
            detail: "accepted volume step lost its acknowledgement".into(),
        })
    }
}

#[test]
fn accepted_room_and_group_steps_never_repeat_after_a_lost_acknowledgement() {
    for s2 in [false, true] {
        for (scope, action) in [
            (VolumeScope::Room, "SetRelativeVolume"),
            (VolumeScope::Group, "SetRelativeGroupVolume"),
        ] {
            let rig = Rig::new(s2);
            let transport = LostStep {
                lan: rig.sim.lan(),
                action,
                writes: Cell::new(0),
            };
            let command = rig.volume(scope, VolumeChange::Adjust(5));
            let failure = execute_healing(
                &transport,
                &rig.houses,
                &guard(&Client::Cli, Policy::default()),
                &command,
                || panic!("an accepted relative step must not survey or repeat"),
            )
            .unwrap_err();
            assert_eq!(failure.code, ErrorCode::PlayerUnreachable);
            assert!(failure.hint.contains("not repeated"), "{failure:?}");
            assert_eq!(transport.writes.get(), 1);
            assert_eq!(soap::get_volume(&rig.sim.lan(), rig.host("Target")).unwrap(), 15);
            let leader = if scope == VolumeScope::Group { 15 } else { 10 };
            assert_eq!(soap::get_volume(&rig.sim.lan(), rig.host("Leader")).unwrap(), leader);
            assert_eq!(soap::get_volume(&rig.sim.lan(), rig.host("Other")).unwrap(), 60);
        }
    }
}
