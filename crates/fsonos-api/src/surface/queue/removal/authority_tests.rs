//! Exercise the shared deletion through both API editing and logged undo.

use super::*;
use crate::surface::queue::QueueAddRequest;
use fsonos_core::HouseholdState;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::Policy;
use fsonos_core::store::MemStore;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::PlayerId;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy)]
enum Fault {
    LostReply,
    ConcurrentBefore,
    ForeignIdenticalRemoval,
    HandoffAfter,
}

struct Replies {
    lan: SimLan,
    fault: Fault,
    heir: PlayerId,
    writes: Arc<AtomicUsize>,
}

fn uris(names: &[&str]) -> Vec<String> {
    names
        .iter()
        .map(|name| format!("x-file-cifs://example.invalid/{name}.flac"))
        .collect()
}

fn lost() -> ProtoError {
    ProtoError::Network {
        target: "synthetic-player".into(),
        detail: "lost queue removal acknowledgement".into(),
    }
}

impl Transport for Replies {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if !action.contains("#RemoveTrackRangeFromQueue") {
            return self.lan.soap_post(host, path, action, body);
        }
        self.writes.fetch_add(1, Ordering::SeqCst);
        if matches!(self.fault, Fault::ConcurrentBefore) {
            soap::enqueue_uri(&self.lan, host, &uris(&["other"])[0], "", 1, false)?;
        }
        if matches!(self.fault, Fault::ForeignIdenticalRemoval) {
            // Another controller removes the same selection. This client's
            // explicit refusal must not be inferred into a recovered success.
            let before = read(&self.lan, host)?;
            soap::remove_track_range_from_queue(&self.lan, host, before.revision, 2, 1)?;
            return Err(ProtoError::SoapFault {
                code: 701,
                reason: "the original removal was refused".into(),
            });
        }
        let result = self.lan.soap_post(host, path, action, body)?;
        match self.fault {
            Fault::LostReply => Err(lost()),
            Fault::HandoffAfter => {
                soap::delegate_group(&self.lan, host, &self.heir)?;
                Err(lost())
            }
            _ => Ok(result),
        }
    }
}

fn setup(s2: bool, fault: Fault) -> (SimHandle, Surface, Arc<AtomicUsize>) {
    let model = if s2 {
        SimModel::One
    } else {
        SimModel::Play5Gen1
    };
    let players = [
        SimPlayerSpec::new("Alpha", model),
        SimPlayerSpec::new("Beta", model),
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
    let alpha = sim.player("Alpha").unwrap();
    let beta = sim.player("Beta").unwrap();
    soap::join_group(&lan, beta.ip, &PlayerId(alpha.uuid.clone())).unwrap();
    let heir = PlayerId(beta.uuid.clone());
    for uri in uris(&["a", "b", "c", "d"]) {
        soap::enqueue_uri(&lan, alpha.ip, &uri, "", 0, false).unwrap();
    }
    let mut household = HouseholdState::default();
    household.apply_topology(&get_zone_group_state(&lan, alpha.ip).unwrap());
    let writes = Arc::new(AtomicUsize::new(0));
    let surface = Surface::new(
        Box::new(Replies {
            lan,
            fault,
            heir,
            writes: Arc::clone(&writes),
        }),
        Box::new(move |_| Ok(vec![household.clone()])),
        Policy::default(),
        Box::new(SystemClock),
    )
    .with_action_log(Box::new(MemStore::default()), "test");
    (sim, surface, writes)
}

fn add(surface: &Surface) {
    let added = surface
        .queue_add(
            &Client::Cli,
            &QueueAddRequest {
                zone: "Beta".into(),
                source_uri: "spotify:track:0SimNewTrack0000000001".into(),
                title: Some("Synthetic queue addition".into()),
                next: false,
            },
        )
        .unwrap();
    assert_eq!((added.first, added.count), (5, 1));
}

fn current(sim: &SimHandle) -> Vec<String> {
    read(&sim.lan(), sim.player("Alpha").unwrap().ip)
        .unwrap()
        .uris
}

#[test]
fn logged_enqueue_undo_completes_after_a_lost_removal_reply() {
    for s2 in [false, true] {
        let (sim, surface, writes) = setup(s2, Fault::LostReply);
        add(&surface);
        let pending = surface.preview_undo(&Client::Cli).unwrap().unwrap();
        let done = surface.undo_checked(&Client::Cli, pending.id).unwrap();
        assert_eq!(done.undone, Some(pending.id));
        assert!(done.failures.is_empty(), "{done:?}");
        assert_eq!(current(&sim), uris(&["a", "b", "c", "d"]));
        assert_eq!(writes.load(Ordering::SeqCst), 1);
        assert!(surface.undo_checked(&Client::Cli, pending.id).is_err());
        assert_eq!(writes.load(Ordering::SeqCst), 1);
        sim.shutdown();
    }
}

#[test]
fn logged_undo_reports_revision_conflict_without_removing_any_selected_music() {
    for s2 in [false, true] {
        let (sim, surface, writes) = setup(s2, Fault::ConcurrentBefore);
        add(&surface);
        let before = current(&sim);
        let pending = surface.preview_undo(&Client::Cli).unwrap().unwrap();
        let done = surface.undo_checked(&Client::Cli, pending.id).unwrap();
        assert_eq!(done.undone, Some(pending.id));
        assert!(
            !done.failures.is_empty(),
            "the failed restoration is structured"
        );
        let mut expected = uris(&["other"]);
        expected.extend(before);
        assert_eq!(current(&sim), expected);
        assert_eq!(writes.load(Ordering::SeqCst), 1);
        sim.shutdown();
    }
}

#[test]
fn explicit_refusal_is_not_recovered_from_another_clients_identical_edit() {
    for s2 in [false, true] {
        let (sim, surface, writes) = setup(s2, Fault::ForeignIdenticalRemoval);
        let error = surface
            .queue_remove(
                &Client::Cli,
                &QueueRemoveRequest {
                    zone: "Beta".into(),
                    position: 2,
                    expected_uris: uris(&["b"]),
                    expected_revision: None,
                },
            )
            .unwrap_err();
        assert_eq!(
            (error.code, error.upnp_code),
            (ErrorCode::UpnpFault, Some(701))
        );
        assert!(error.detail.contains("original removal was refused"));
        assert!(!error.retryable());
        assert_eq!(current(&sim), uris(&["a", "c", "d"]));
        assert_eq!(writes.load(Ordering::SeqCst), 1);
        sim.shutdown();
    }
}

#[test]
fn a_handoff_during_removal_cannot_confirm_the_old_coordinators_result() {
    for s2 in [false, true] {
        let (sim, surface, writes) = setup(s2, Fault::HandoffAfter);
        let error = surface
            .queue_remove(
                &Client::Cli,
                &QueueRemoveRequest {
                    zone: "Beta".into(),
                    position: 2,
                    expected_uris: uris(&["b"]),
                    expected_revision: None,
                },
            )
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::OutcomeUncertain);
        assert!(error.detail.contains("uncertain"));
        assert_eq!(writes.load(Ordering::SeqCst), 1);
        sim.shutdown();
    }
}

#[test]
fn removal_errors_keep_the_existing_wire_categories() {
    let invalid = removal_failure(RemovalError::Invalid("stale range"));
    assert_eq!(
        (invalid.code, invalid.status()),
        (ErrorCode::InvalidArgument, 422)
    );
    let before = removal_failure(RemovalError::Before(lost()));
    assert_eq!(before.code, ErrorCode::PlayerUnreachable);
    let uncertain = removal_failure(RemovalError::Uncertain("read-back changed".into()));
    assert_eq!(
        (uncertain.code, uncertain.status()),
        (ErrorCode::OutcomeUncertain, 502)
    );
    assert!(!uncertain.retryable());
    assert!(uncertain.hint.contains("do not retry blindly"));
}
