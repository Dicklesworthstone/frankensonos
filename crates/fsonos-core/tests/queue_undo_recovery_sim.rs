//! Enqueue undo uses the native queue revision and observes lost replies.
//! All players and resources are synthetic; only loopback sockets are used.

use fsonos_core::{HouseholdState, control};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::PlayerId;
use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy)]
enum Fault {
    None,
    LostReply,
    MalformedReply,
    ConcurrentBefore,
    ConcurrentAfter,
    LostBefore,
}

struct Replies {
    lan: SimLan,
    fault: Fault,
    writes: AtomicUsize,
}

fn uris(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| format!("x-file-cifs://example.invalid/{name}.flac")).collect()
}

fn lost() -> ProtoError {
    ProtoError::Network { target: "synthetic-player".into(), detail: "removal reply lost".into() }
}

impl Transport for Replies {
    fn soap_post(&self, host: IpAddr, path: &str, action: &str, body: &str) -> Result<String, ProtoError> {
        if !action.contains("#RemoveTrackRangeFromQueue") {
            return self.lan.soap_post(host, path, action, body);
        }
        self.writes.fetch_add(1, Ordering::SeqCst);
        if matches!(self.fault, Fault::LostBefore) {
            return Err(lost());
        }
        if matches!(self.fault, Fault::ConcurrentBefore) {
            soap::enqueue_uri(&self.lan, host, &uris(&["other"])[0], "", 1, false)?;
        }
        let response = self.lan.soap_post(host, path, action, body)?;
        match self.fault {
            Fault::LostReply => Err(lost()),
            Fault::MalformedReply => Ok("unreadable SOAP reply".into()),
            Fault::ConcurrentAfter => {
                soap::enqueue_uri(&self.lan, host, &uris(&["other"])[0], "", 0, false)?;
                Err(lost())
            }
            _ => Ok(response),
        }
    }
}

fn setup(s2: bool, fault: Fault) -> (SimHandle, Replies, Vec<HouseholdState>, PlayerId, IpAddr) {
    let model = if s2 { SimModel::One } else { SimModel::Play5Gen1 };
    let players = [SimPlayerSpec::new("Synthetic", model)];
    let builder = SimHousehold::builder();
    let sim = if s2 { builder.s2(players) } else { builder.s1(players) }.spawn().unwrap();
    let player = sim.player("Synthetic").unwrap();
    let (host, id) = (player.ip, PlayerId(player.uuid.clone()));
    let lan = sim.lan();
    for uri in uris(&["a", "b", "b", "d"]) {
        soap::enqueue_uri(&lan, host, &uri, "", 0, false).unwrap();
    }
    let mut household = HouseholdState::default();
    household.apply_topology(&get_zone_group_state(&lan, host).unwrap());
    let wire = Replies { lan, fault, writes: AtomicUsize::new(0) };
    (sim, wire, vec![household], id, host)
}

fn current(sim: &SimHandle, host: IpAddr) -> Vec<String> {
    control::queue_removal::read(&sim.lan(), host).unwrap().uris
}

#[test]
fn undo_preserves_surrounding_tracks_and_recovers_lost_replies_on_both_generations() {
    for s2 in [false, true] {
        for fault in [Fault::None, Fault::LostReply, Fault::MalformedReply] {
            let (sim, wire, households, id, host) = setup(s2, fault);
            let before = sim.soap_log().len();
            assert!(control::unqueue(&wire, &households, &id, 2, &uris(&["b", "b"])).unwrap());
            assert_eq!(current(&sim, host), uris(&["a", "d"]));
            assert_eq!(wire.writes.load(Ordering::SeqCst), 1);
            let log = sim.soap_log();
            assert!(log[before..].iter().all(|entry| {
                entry.action == "Browse" || entry.action == "GetZoneGroupState"
                    || entry.action == "RemoveTrackRangeFromQueue"
            }));
            sim.shutdown();
        }
    }
}

#[test]
fn native_revision_refuses_a_concurrent_insertion_instead_of_deleting_other_music() {
    for s2 in [false, true] {
        let (sim, wire, households, id, host) = setup(s2, Fault::ConcurrentBefore);
        assert!(control::unqueue(&wire, &households, &id, 2, &uris(&["b", "b"])).is_err());
        assert_eq!(wire.writes.load(Ordering::SeqCst), 1);
        assert_eq!(current(&sim, host), uris(&["other", "a", "b", "b", "d"]));
        sim.shutdown();
    }
}

#[test]
fn missing_selection_is_not_searched_for_or_removed_elsewhere() {
    for s2 in [false, true] {
        let (sim, wire, households, id, host) = setup(s2, Fault::None);
        assert!(!control::unqueue(&wire, &households, &id, 1, &uris(&["b", "b"])).unwrap());
        assert!(!control::unqueue(&wire, &households, &id, 4, &uris(&["d", "b"])).unwrap());
        assert_eq!(wire.writes.load(Ordering::SeqCst), 0);
        assert_eq!(current(&sim, host), uris(&["a", "b", "b", "d"]));
        sim.shutdown();
    }
}

#[test]
fn incomplete_or_concurrent_effects_do_not_become_success_or_a_second_write() {
    for s2 in [false, true] {
        for (fault, expected) in [
            (Fault::LostBefore, vec!["a", "b", "b", "d"]),
            (Fault::ConcurrentAfter, vec!["a", "d", "other"]),
        ] {
            let (sim, wire, households, id, host) = setup(s2, fault);
            let error = control::unqueue(&wire, &households, &id, 2, &uris(&["b", "b"])).unwrap_err();
            assert!(error.to_string().contains("uncertain"));
            assert_eq!(wire.writes.load(Ordering::SeqCst), 1);
            assert_eq!(current(&sim, host), uris(&expected));
            sim.shutdown();
        }
    }
}
