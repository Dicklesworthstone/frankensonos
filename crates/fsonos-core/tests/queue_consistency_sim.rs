//! Queue snapshots and undo while another controller edits a real simulator.
//! All SOAP exchanges use localhost; synthetic HTTP URIs are queued, not played.

use fsonos_core::{HouseholdState, control};
use fsonos_proto::content::{self, QUEUE};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use fsonos_types::PlayerId;
use std::cell::RefCell;
use std::net::IpAddr;

fn household() -> SimHandle {
    SimHousehold::builder()
        .s1([SimPlayerSpec::new("Legacy", SimModel::Play5Gen1)])
        .s2([SimPlayerSpec::new("Modern", SimModel::One)])
        .spawn()
        .unwrap()
}

fn seed(t: &impl Transport, host: IpAddr, count: u32) -> Vec<String> {
    (0..count)
        .map(|n| {
            let uri = format!("http://example.invalid/queue/{n}.mp3");
            soap::enqueue_uri(t, host, &uri, "", 0, false).unwrap();
            uri
        })
        .collect()
}

fn uris(t: &impl Transport, host: IpAddr) -> Vec<String> {
    content::browse_all(t, host, QUEUE)
        .unwrap()
        .into_iter()
        .map(|o| o.res.unwrap().uri)
        .collect()
}

fn state(t: &impl Transport, host: IpAddr) -> Vec<HouseholdState> {
    let mut house = HouseholdState::default();
    house.apply_topology(&get_zone_group_state(t, host).unwrap());
    vec![house]
}

/// Act as a second controller after the selected SOAP response, before the
/// client sees it. The edit and the response both come from real sockets.
struct Editing<'a, T, F> {
    inner: &'a T,
    action: &'static str,
    edit: RefCell<Option<F>>,
}

impl<T: Transport, F: FnOnce()> Transport for Editing<'_, T, F> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let response = self.inner.soap_post(host, path, action, body)?;
        if action.trim_matches('"').rsplit('#').next() == Some(self.action) {
            let edit = self.edit.borrow_mut().take();
            if let Some(edit) = edit {
                edit();
            }
        }
        Ok(response)
    }
}

const REPLACEMENT: &str = "http://example.invalid/queue/replacement.mp3";

fn replace_first(t: &impl Transport, host: IpAddr) {
    soap::remove_track_range_from_queue(t, host, 0, 1, 1).unwrap();
    soap::enqueue_uri(t, host, REPLACEMENT, "", 1, false).unwrap();
}

#[test]
fn paged_snapshots_restart_after_same_length_edits_on_both_generations() {
    let sim = household();
    let lan = sim.lan();
    for room in ["Legacy", "Modern"] {
        let player = sim.player(room).unwrap();
        let mut expected = seed(&lan, player.ip, content::PAGE_SIZE + 1);
        let editing = Editing {
            inner: &lan,
            action: "Browse",
            edit: RefCell::new(Some(|| replace_first(&lan, player.ip))),
        };
        let found = uris(&editing, player.ip);
        expected[0] = REPLACEMENT.into();
        assert_eq!(found, expected);
        assert!(editing.edit.borrow().is_none());
        assert_eq!(uris(&lan, player.ip), expected);
    }
    sim.shutdown();
}

#[test]
fn undo_of_a_large_range_preserves_unrelated_tracks_on_both_generations() {
    let sim = household();
    let lan = sim.lan();
    for room in ["Legacy", "Modern"] {
        let player = sim.player(room).unwrap();
        let expected = seed(&lan, player.ip, content::PAGE_SIZE + 5);
        let houses = state(&lan, player.ip);
        let id = PlayerId(player.uuid.clone());
        // Leave two owner's tracks on each side of a 101-track addition.
        let end = expected.len() - 2;
        assert!(control::unqueue(&lan, &houses, &id, 3, &expected[2..end]).unwrap());
        let remaining: Vec<String> = expected[..2]
            .iter()
            .chain(&expected[end..])
            .cloned()
            .collect();
        assert_eq!(uris(&lan, player.ip), remaining);
    }
    sim.shutdown();
}

#[test]
fn an_edit_between_undo_pages_never_authorizes_removing_the_new_range() {
    let sim = household();
    let lan = sim.lan();
    for room in ["Legacy", "Modern"] {
        let player = sim.player(room).unwrap();
        let mut expected = seed(&lan, player.ip, content::PAGE_SIZE + 1);
        let houses = state(&lan, player.ip);
        let id = PlayerId(player.uuid.clone());
        let editing = Editing {
            inner: &lan,
            action: "Browse",
            edit: RefCell::new(Some(|| replace_first(&lan, player.ip))),
        };
        assert!(!control::unqueue(&editing, &houses, &id, 1, &expected).unwrap());
        expected[0] = REPLACEMENT.into();
        assert_eq!(uris(&lan, player.ip), expected);
        let removals = sim
            .soap_log()
            .iter()
            .filter(|e| e.room == room && e.action == "RemoveTrackRangeFromQueue")
            .count();
        assert_eq!(removals, 1);
    }
    sim.shutdown();
}

#[test]
fn a_later_read_failure_cannot_remove_a_partially_verified_queue_range() {
    let sim = household();
    let lan = sim.lan();
    for room in ["Legacy", "Modern"] {
        let player = sim.player(room).unwrap();
        let expected = seed(&lan, player.ip, content::PAGE_SIZE + 1);
        let houses = state(&lan, player.ip);
        let id = PlayerId(player.uuid.clone());
        let editing = Editing {
            inner: &lan,
            action: "Browse",
            edit: RefCell::new(Some(|| sim.upnp_fault(room, "Browse", 701).unwrap())),
        };
        assert!(control::unqueue(&editing, &houses, &id, 1, &expected).is_err());
        sim.clear_faults(room).unwrap();
        assert_eq!(uris(&lan, player.ip), expected);
        assert!(
            sim.soap_log()
                .iter()
                .all(|e| e.room != room || e.action != "RemoveTrackRangeFromQueue")
        );
    }
    sim.shutdown();
}

#[test]
fn truncated_enqueue_readback_is_not_success_and_does_not_enqueue_twice() {
    let sim = household();
    let lan = sim.lan();
    for room in ["Legacy", "Modern"] {
        let player = sim.player(room).unwrap();
        let houses = state(&lan, player.ip);
        let id = PlayerId(player.uuid.clone());
        let editing = Editing {
            inner: &lan,
            action: "AddURIToQueue",
            edit: RefCell::new(Some(|| {
                soap::remove_all_tracks_from_queue(&lan, player.ip).unwrap();
            })),
        };
        let result = control::enqueue_spotify(
            &editing,
            &houses,
            &id,
            "spotify:track:ReadbackTest000001",
            "Read-back test",
            false,
        );
        assert!(result.is_err());
        assert!(uris(&lan, player.ip).is_empty());
        let adds = sim
            .soap_log()
            .iter()
            .filter(|e| e.room == room && e.action == "AddURIToQueue")
            .count();
        assert_eq!(adds, 1);
    }
    sim.shutdown();
}
