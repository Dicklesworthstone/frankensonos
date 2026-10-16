//! Real simulator sockets exercise recovery across the entire command, not
//! only the inner source-start helper that already avoids replaying writes.

use fsonos_api::heal::execute_healing;
use fsonos_api::plan::{TransportAction, VolumeScope};
use fsonos_api::{Command, ErrorCode, Guard, NoteCode, VolumeChange};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::{HouseholdState, control, resolve_room};
use fsonos_proto::soap;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::PlayerId;
use std::cell::{Cell, RefCell};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn grouped() -> (SimHandle, Vec<HouseholdState>, PlayerId) {
    let sim = SimHousehold::builder()
        .s1([
            SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1),
            SimPlayerSpec::new("Office", SimModel::Play5Gen1),
        ])
        .spawn()
        .unwrap();
    let lan = sim.lan();
    let houses = survey(&lan);
    let kitchen = resolve_room(&houses, "Kitchen").unwrap().player.id.clone();
    let office = resolve_room(&houses, "Office").unwrap().player.id.clone();
    control::join(&lan, &houses, &office, &kitchen).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let host = sim.player("Kitchen").unwrap().ip;
    while !fsonos_proto::topology::get_zone_group_state(&lan, host).is_ok_and(|z| {
        z.groups
            .iter()
            .any(|g| g.coordinator == kitchen && g.members.len() == 2)
    }) {
        assert!(Instant::now() < deadline, "Office never joined Kitchen");
        std::thread::sleep(Duration::from_millis(50));
    }
    let houses = survey(&lan);
    assert_eq!(
        resolve_room(&houses, "Office").unwrap().coordinator.id,
        kitchen
    );
    (sim, houses, kitchen)
}

fn survey(t: &impl Transport) -> Vec<HouseholdState> {
    fsonos_core::inventory::survey(t, &[], Duration::from_millis(500))
        .unwrap()
        .households
}

fn guard() -> Guard<'static> {
    Guard {
        policy: Arc::new(Policy::default()),
        client: &Client::Cli,
        clock: &SystemClock,
    }
}

fn play(coordinator: &PlayerId) -> Command {
    Command::Play {
        coordinator: coordinator.clone(),
        source_uri: "http://example.invalid/synthetic-radio".into(),
        title: Some("Synthetic station".into()),
    }
}

/// Change coordinator only AFTER the real SetAVTransportURI was accepted.
struct Handoff<'a> {
    sim: &'a SimHandle,
    lan: SimLan,
    switched: Cell<bool>,
    calls: RefCell<Vec<String>>,
}

impl Transport for Handoff<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        header: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let action = header.trim_matches('"').rsplit('#').next().unwrap();
        self.calls.borrow_mut().push(action.to_string());
        let reply = self.lan.soap_post(host, path, header, body)?;
        if action == "SetAVTransportURI"
            && soap::parse_response(&reply, action).is_ok()
            && !self.switched.replace(true)
        {
            self.sim.reelect_coordinator("Kitchen").unwrap();
        }
        Ok(reply)
    }
}

#[test]
fn a_late_800_does_not_replay_the_accepted_source_on_a_new_coordinator() {
    let (sim, houses, kitchen) = grouped();
    let t = Handoff {
        sim: &sim,
        lan: sim.lan(),
        switched: Cell::new(false),
        calls: RefCell::new(Vec::new()),
    };
    let failure = execute_healing(&t, &houses, &guard(), &play(&kitchen), || {
        panic!("late source failure must not start another recovery")
    })
    .unwrap_err();
    assert_eq!(failure.upnp_code, Some(800));
    assert!(failure.hint.contains("earlier speaker step"), "{failure:?}");
    let calls = t.calls.borrow();
    assert_eq!(
        calls.iter().filter(|a| *a == "SetAVTransportURI").count(),
        1
    );
    assert_eq!(calls.iter().filter(|a| *a == "Play").count(), 1);
}

struct LostPlay {
    lan: SimLan,
    calls: RefCell<Vec<String>>,
}

impl Transport for LostPlay {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        header: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let action = header.trim_matches('"').rsplit('#').next().unwrap();
        self.calls.borrow_mut().push(action.to_string());
        if action == "Play" || action == "GetMediaInfo" {
            return Err(ProtoError::Network {
                target: "synthetic".into(),
                detail: "reply lost".into(),
            });
        }
        self.lan.soap_post(host, path, header, body)
    }
}

#[test]
fn source_success_then_lost_play_and_readback_does_not_restart_at_a_new_address() {
    let (sim, houses, kitchen) = grouped();
    let t = LostPlay {
        lan: sim.lan(),
        calls: RefCell::new(Vec::new()),
    };
    let surveys = Cell::new(0);
    let failure = execute_healing(&t, &houses, &guard(), &play(&kitchen), || {
        surveys.set(surveys.get() + 1);
        None
    })
    .unwrap_err();
    assert_eq!(failure.code, ErrorCode::PlayerUnreachable);
    assert_eq!(surveys.get(), 0, "do not enter the outer readdress retry");
    assert!(failure.hint.contains("not repeated"));
    assert_eq!(
        t.calls
            .borrow()
            .iter()
            .filter(|a| *a == "SetAVTransportURI")
            .count(),
        1
    );
}

/// Counts group volume attempts by the SnapshotGroupVolume each one opens
/// with: the old coordinator refuses that with 800, so its attempt never
/// reaches SetGroupVolume. The new coordinator's SetGroupVolume is lost.
struct LostOnNewCoordinator {
    lan: SimLan,
    new: IpAddr,
    attempts: Cell<usize>,
}

impl Transport for LostOnNewCoordinator {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        header: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if header.ends_with("#SnapshotGroupVolume\"") {
            self.attempts.set(self.attempts.get() + 1);
        }
        if header.ends_with("#SetGroupVolume\"") && host == self.new {
            return Err(ProtoError::Network {
                target: "synthetic".into(),
                detail: "lost at new coordinator".into(),
            });
        }
        self.lan.soap_post(host, path, header, body)
    }
}

#[test]
fn coordinator_retry_cannot_chain_into_a_third_attempt_at_the_original_player() {
    let (sim, houses, kitchen) = grouped();
    sim.reelect_coordinator("Kitchen").unwrap();
    let t = LostOnNewCoordinator {
        lan: sim.lan(),
        new: sim.player("Office").unwrap().ip,
        attempts: Cell::new(0),
    };
    let command = Command::Volume {
        target: kitchen,
        scope: VolumeScope::Group,
        change: VolumeChange::Set(31),
    };
    let surveys = Cell::new(0);
    let failure = execute_healing(&t, &houses, &guard(), &command, || {
        surveys.set(surveys.get() + 1);
        None
    })
    .unwrap_err();
    assert_eq!(failure.code, ErrorCode::PlayerUnreachable);
    assert_eq!(t.attempts.get(), 2, "no third attempt");
    assert_eq!(surveys.get(), 0);
}

#[test]
fn an_initial_coordinator_refusal_still_recovers_and_changes_the_group_volume() {
    let (sim, houses, kitchen) = grouped();
    sim.reelect_coordinator("Kitchen").unwrap();
    let command = Command::Volume {
        target: kitchen,
        scope: VolumeScope::Group,
        change: VolumeChange::Set(31),
    };
    let outcome = execute_healing(&sim.lan(), &houses, &guard(), &command, || {
        panic!("coordinator recovery already used its retry")
    })
    .unwrap();
    assert!(
        outcome
            .notes
            .iter()
            .any(|note| note.code == NoteCode::Healed)
    );
    assert_eq!(
        fsonos_proto::control::get_group_volume(&sim.lan(), sim.player("Office").unwrap().ip)
            .unwrap(),
        31
    );
}

#[test]
fn a_single_repeat_safe_write_still_recovers_at_the_players_new_address() {
    let (mut sim, houses, kitchen) = grouped();
    let new = sim.change_address("Kitchen").unwrap();
    let command = Command::Mute {
        target: kitchen,
        mute: true,
    };
    let outcome = execute_healing(&sim.lan(), &houses, &guard(), &command, || {
        Some(survey(&sim.lan()))
    })
    .unwrap();
    assert!(
        outcome
            .notes
            .iter()
            .any(|note| note.code == NoteCode::Healed)
    );
    assert!(fsonos_proto::control::get_mute(&sim.lan(), new).unwrap());
}

#[test]
fn a_relative_step_with_a_lost_reply_does_not_start_address_recovery() {
    let (mut sim, houses, kitchen) = grouped();
    sim.change_address("Kitchen").unwrap();
    let command = Command::Transport {
        coordinator: kitchen,
        action: TransportAction::Next,
    };
    let failure = execute_healing(&sim.lan(), &houses, &guard(), &command, || {
        panic!("Next must never be retried after a lost reply")
    })
    .unwrap_err();
    assert_eq!(failure.code, ErrorCode::PlayerUnreachable);
}

struct LostAcknowledgement {
    lan: SimLan,
    lose: &'static str,
    lost: Cell<bool>,
    calls: RefCell<Vec<String>>,
}

impl Transport for LostAcknowledgement {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        header: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let action = header.trim_matches('"').rsplit('#').next().unwrap();
        self.calls.borrow_mut().push(action.to_string());
        let reply = self.lan.soap_post(host, path, header, body)?;
        if action == self.lose
            && soap::parse_response(&reply, action).is_ok()
            && !self.lost.replace(true)
        {
            return Err(ProtoError::Network {
                target: "synthetic".into(),
                detail: "accepted but reply lost".into(),
            });
        }
        Ok(reply)
    }
}

#[test]
fn verified_lost_acknowledgements_still_complete_without_replaying_either_stage() {
    for lose in ["SetAVTransportURI", "Play"] {
        let (sim, houses, kitchen) = grouped();
        let t = LostAcknowledgement {
            lan: sim.lan(),
            lose,
            lost: Cell::new(false),
            calls: RefCell::new(Vec::new()),
        };
        let outcome = execute_healing(&t, &houses, &guard(), &play(&kitchen), || {
            panic!("read-back, not a whole-command retry, confirms the accepted write")
        })
        .unwrap();
        assert!(outcome.changed);
        let calls = t.calls.borrow();
        assert_eq!(
            calls.iter().filter(|a| *a == "SetAVTransportURI").count(),
            1
        );
        assert_eq!(calls.iter().filter(|a| *a == "Play").count(), 1);
        assert!(calls.iter().any(|a| a == "GetMediaInfo"));
    }
}
