use super::*;
use fsonos_core::HouseholdState;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::Policy;
use fsonos_core::store::{ActionFilter, MemStore};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::PlayerId;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

fn queue(revision: u32, uris: &[&str]) -> Queue {
    Queue {
        revision,
        uris: uris.iter().map(|s| (*s).to_string()).collect(),
    }
}

#[test]
fn confirmation_requires_exact_deletion_and_a_new_revision() {
    let before = queue(5, &["a", "b", "c", "d"]);
    assert!(removed(&before, &queue(6, &["a", "d"]), 1, 3));
    assert!(removed(&before, &queue(6, &["c", "d"]), 0, 2));
    assert!(removed(&before, &queue(6, &["a", "b"]), 2, 4));
    assert!(removed(&before, &queue(6, &[]), 0, 4));
    for after in [
        queue(5, &["a", "d"]),
        queue(6, &["a", "c"]),
        queue(6, &["a", "d", "x"]),
        queue(6, &["a"]),
        queue(7, &["a", "b", "c", "d"]),
    ] {
        assert!(!removed(&before, &after, 1, 3));
    }
    assert!(!removed(&before, &queue(6, &[]), 0, 5));
    assert!(!removed(&before, &before, 2, 2));
    assert!(!removed(&before, &before, 3, 2));
}

#[test]
fn duplicates_and_revision_wrap_do_not_depend_on_uniqueness_or_ordering() {
    let before = queue(u32::MAX, &["x", "x", "x"]);
    assert!(removed(&before, &queue(0, &["x", "x"]), 1, 2));
    assert!(!removed(&before, &queue(0, &["x"]), 1, 2));
    assert!(!removed(&before, &before, 1, 2));
}

fn invalid_requests() -> Vec<QueueRemoveRequest> {
    let valid = request(2, &["b"]);
    vec![
        QueueRemoveRequest {
            zone: " ".into(),
            ..valid.clone()
        },
        QueueRemoveRequest {
            position: 0,
            ..valid.clone()
        },
        QueueRemoveRequest {
            expected_uris: vec![],
            ..valid.clone()
        },
        QueueRemoveRequest {
            expected_uris: vec![" ".into()],
            ..valid.clone()
        },
        QueueRemoveRequest {
            expected_uris: vec!["NOT_IMPLEMENTED".into()],
            ..valid.clone()
        },
        QueueRemoveRequest {
            expected_uris: vec!["uri".into(); 101],
            ..valid.clone()
        },
        QueueRemoveRequest {
            position: u32::MAX,
            expected_uris: vec!["uri".into(); 2],
            ..valid.clone()
        },
        QueueRemoveRequest {
            expected_revision: Some(" ".into()),
            ..valid
        },
    ]
}

#[test]
fn request_validation_checks_the_whole_range_and_rejects_unknown_fields() {
    assert_eq!(request(2, &["b"]).range().unwrap(), (1, 2));
    for request in invalid_requests() {
        assert!(request.range().is_err());
    }
    for json in [
        r#"{"zone":"Alpha","position":1}"#,
        r#"{"zone":"Alpha","position":1,"expected_uris":["uri"],"force":true}"#,
    ] {
        assert!(serde_json::from_str::<QueueRemoveRequest>(json).is_err());
    }
}

#[derive(Clone, Copy)]
enum Fault {
    None,
    BeforeWrite,
    LostReply,
    MalformedReply,
    UnreadableAfter,
    ConcurrentBefore,
    ConcurrentAfter,
    Rejected,
    ZeroRevision,
    UnreadableBefore,
}

struct Replies {
    lan: SimLan,
    fault: Fault,
    attempts: Arc<AtomicUsize>,
    unreadable: AtomicBool,
}

fn lost(host: IpAddr) -> ProtoError {
    ProtoError::Network {
        target: host.to_string(),
        detail: "synthetic lost removal reply".into(),
    }
}

fn insert_other(lan: &SimLan, host: IpAddr) -> Result<(), ProtoError> {
    soap::enqueue_uri(
        lan,
        host,
        "x-file-cifs://example.invalid/new.flac",
        "",
        0,
        false,
    )
    .map(drop)
}

impl Transport for Replies {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if action.contains("#Browse") {
            if self.unreadable.load(Ordering::Acquire)
                || matches!(self.fault, Fault::UnreadableBefore)
            {
                return Err(lost(host));
            }
            if matches!(self.fault, Fault::ZeroRevision) {
                return self.lan.soap_post(host, path, action, body).map(|reply| {
                    reply.replace("<UpdateID>4</UpdateID>", "<UpdateID>0</UpdateID>")
                });
            }
        }
        if !action.contains("#RemoveTrackRangeFromQueue") {
            return self.lan.soap_post(host, path, action, body);
        }
        self.attempts.fetch_add(1, Ordering::AcqRel);
        match self.fault {
            Fault::BeforeWrite => return Err(lost(host)),
            Fault::Rejected => {
                return Err(ProtoError::SoapFault {
                    code: 701,
                    reason: "synthetic rejection".into(),
                });
            }
            Fault::ConcurrentBefore => insert_other(&self.lan, host)?,
            _ => {}
        }
        let response = self.lan.soap_post(host, path, action, body)?;
        match self.fault {
            Fault::LostReply => Err(lost(host)),
            Fault::MalformedReply => Ok("not a SOAP response".into()),
            Fault::UnreadableAfter => {
                self.unreadable.store(true, Ordering::Release);
                Err(lost(host))
            }
            Fault::ConcurrentAfter => {
                insert_other(&self.lan, host)?;
                Err(lost(host))
            }
            _ => Ok(response),
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
    soap::join_group(
        &lan,
        sim.player("Beta").unwrap().ip,
        &PlayerId(alpha.uuid.clone()),
    )
    .unwrap();
    for uri in request(1, &["a", "b", "c", "d"]).expected_uris {
        soap::enqueue_uri(&lan, alpha.ip, &uri, "", 0, false).unwrap();
    }
    let mut household = HouseholdState::default();
    household.apply_topology(&get_zone_group_state(&lan, alpha.ip).unwrap());
    let attempts = Arc::new(AtomicUsize::new(0));
    let surface = Surface::new(
        Box::new(Replies {
            lan: sim.lan(),
            fault,
            attempts: Arc::clone(&attempts),
            unreadable: AtomicBool::new(false),
        }),
        Box::new(move |_| Ok(vec![household.clone()])),
        Policy::default(),
        Box::new(SystemClock),
    )
    .with_action_log(Box::new(MemStore::default()), "test");
    (sim, surface, attempts)
}

fn request(position: u32, names: &[&str]) -> QueueRemoveRequest {
    QueueRemoveRequest {
        zone: "Beta".into(),
        position,
        expected_uris: names
            .iter()
            .map(|name| format!("x-file-cifs://example.invalid/{name}.flac"))
            .collect(),
        expected_revision: None,
    }
}

fn current(sim: &SimHandle) -> Vec<String> {
    read(&sim.lan(), sim.player("Alpha").unwrap().ip)
        .unwrap()
        .uris
}

#[test]
fn removes_ranges_through_a_member_without_rebuilding_or_starting_playback() {
    for s2 in [false, true] {
        for (position, names, left) in [
            (1, vec!["a"], vec!["b", "c", "d"]),
            (2, vec!["b", "c"], vec!["a", "d"]),
            (4, vec!["d"], vec!["a", "b", "c"]),
            (1, vec!["a", "b", "c", "d"], vec![]),
        ] {
            let (sim, surface, attempts) = setup(s2, Fault::None);
            let before = sim.soap_log().len();
            let done = surface
                .queue_remove(&Client::Cli, &request(position, &names))
                .unwrap();
            assert_eq!(done.zone, "Alpha");
            assert_eq!(usize::try_from(done.count).unwrap(), names.len());
            assert_eq!(usize::try_from(done.remaining).unwrap(), left.len());
            assert!(!done.recovered && !done.undoable);
            assert!(done.text().contains("not undoable"));
            assert_eq!(attempts.load(Ordering::Acquire), 1);
            let log = sim.soap_log();
            assert!(log[before..].iter().all(|entry| {
                entry.action.starts_with("Get")
                    || entry.action == "Browse"
                    || entry.action == "RemoveTrackRangeFromQueue"
            }));
            assert_eq!(current(&sim), request(1, &left).expected_uris);
            assert!(surface.undo(&Client::Cli, true).unwrap().is_none());
            sim.shutdown();
        }
    }
}

#[test]
fn stale_or_invalid_selections_do_not_write_and_unknown_callers_do_not_read() {
    let (sim, surface, attempts) = setup(true, Fault::None);
    let before = sim.soap_log().len();
    let denied = surface
        .queue_remove(&Client::Unknown, &request(2, &["b"]))
        .unwrap_err();
    assert_eq!(denied.code, ErrorCode::PolicyDenied);
    assert_eq!(sim.soap_log().len(), before);
    for req in invalid_requests() {
        assert!(surface.queue_remove(&Client::Cli, &req).is_err());
    }
    assert_eq!(
        sim.soap_log().len(),
        before,
        "invalid requests made no reads"
    );
    for req in [request(2, &["a"]), request(4, &["d", "a"])] {
        let error = surface.queue_remove(&Client::Cli, &req).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidArgument);
        assert!(!error.code.retryable());
    }
    assert_eq!(attempts.load(Ordering::Acquire), 0);
    assert_eq!(current(&sim).len(), 4);
    // The CLI's refusals are logged as every failed call is: the policy
    // allowed them, and the result says each failed.
    let log = surface
        .recent_actions(&Client::Cli, &ActionFilter::default())
        .unwrap();
    let refused: Vec<_> = log
        .iter()
        .filter(|e| e.action.client == Client::Cli.key())
        .collect();
    assert!(!refused.is_empty(), "{log:?}");
    for entry in refused {
        assert_eq!(entry.action.decision, "allow", "{entry:?}");
        assert!(entry.action.result.starts_with("failed: "), "{entry:?}");
    }
    sim.shutdown();
}

#[test]
fn lost_and_malformed_replies_are_recovered_without_a_second_removal() {
    for s2 in [false, true] {
        for fault in [Fault::LostReply, Fault::MalformedReply] {
            let (sim, surface, attempts) = setup(s2, fault);
            let done = surface
                .queue_remove(&Client::Cli, &request(2, &["b", "c"]))
                .unwrap();
            assert!(done.recovered);
            assert_eq!(done.remaining, 2);
            assert_eq!(attempts.load(Ordering::Acquire), 1);
            assert_eq!(current(&sim), request(1, &["a", "d"]).expected_uris);
            sim.shutdown();
        }
    }
}

#[test]
fn uncertain_effects_are_reported_not_retried() {
    for s2 in [false, true] {
        for (fault, remaining) in [
            (Fault::BeforeWrite, 4),
            (Fault::UnreadableAfter, 2),
            (Fault::ConcurrentAfter, 3),
        ] {
            let (sim, surface, attempts) = setup(s2, fault);
            let error = surface
                .queue_remove(&Client::Cli, &request(2, &["b", "c"]))
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::UpnpFault);
            assert!(!error.code.retryable());
            assert!(error.detail.contains("uncertain"));
            assert_eq!(attempts.load(Ordering::Acquire), 1);
            assert_eq!(current(&sim).len(), remaining);
            sim.shutdown();
        }
    }
}

#[test]
fn a_native_revision_conflict_preserves_the_concurrent_edit_and_selected_tracks() {
    for s2 in [false, true] {
        let (sim, surface, attempts) = setup(s2, Fault::ConcurrentBefore);
        assert!(
            surface
                .queue_remove(&Client::Cli, &request(2, &["b", "c"]))
                .is_err()
        );
        assert_eq!(attempts.load(Ordering::Acquire), 1);
        let now = current(&sim);
        assert_eq!(now[..4], request(1, &["a", "b", "c", "d"]).expected_uris);
        assert_eq!(now.len(), 5);
        sim.shutdown();
    }
}

#[test]
fn a_rejection_with_an_unchanged_revision_keeps_the_original_error() {
    let (sim, surface, attempts) = setup(true, Fault::Rejected);
    let error = surface
        .queue_remove(&Client::Cli, &request(2, &["b"]))
        .unwrap_err();
    assert!(!error.detail.contains("uncertain"));
    assert_eq!(attempts.load(Ordering::Acquire), 1);
    assert_eq!(current(&sim).len(), 4);
    sim.shutdown();
}

#[test]
fn unreadable_preflight_and_wildcard_revision_never_send_a_removal() {
    for fault in [Fault::ZeroRevision, Fault::UnreadableBefore] {
        let (sim, surface, attempts) = setup(true, fault);
        assert!(
            surface
                .queue_remove(&Client::Cli, &request(2, &["b"]))
                .is_err()
        );
        assert_eq!(attempts.load(Ordering::Acquire), 0);
        assert_eq!(current(&sim).len(), 4);
        sim.shutdown();
    }
}

#[test]
fn fresh_topology_does_not_remove_from_a_former_coordinators_queue() {
    for s2 in [false, true] {
        let (sim, surface, attempts) = setup(s2, Fault::None);
        let beta = sim.player("Beta").unwrap();
        soap::leave_group(&sim.lan(), beta.ip, &PlayerId(beta.uuid.clone())).unwrap();
        let before = sim.soap_log().len();
        assert!(
            surface
                .queue_remove(&Client::Cli, &request(2, &["b"]))
                .is_err()
        );
        assert_eq!(attempts.load(Ordering::Acquire), 0);
        assert_eq!(current(&sim).len(), 4);
        assert!(
            sim.soap_log()[before..]
                .iter()
                .all(|entry| entry.action.starts_with("Get") || entry.action == "Browse")
        );
        sim.shutdown();
    }
}

#[test]
fn revision_tokens_bind_removal_to_the_queue_observed_by_the_client() {
    for s2 in [false, true] {
        let (sim, surface, attempts) = setup(s2, Fault::None);
        let mut req = request(2, &["b"]);
        req.expected_revision = surface
            .queue(&Client::Cli, "Beta", 0, 100)
            .unwrap()
            .revision;
        assert!(req.expected_revision.is_some());
        let mut stale = req.clone();
        stale.expected_revision = Some("another coordinator or revision".into());
        assert!(surface.queue_remove(&Client::Cli, &stale).is_err());
        assert_eq!(attempts.load(Ordering::Acquire), 0);
        let done = surface.queue_remove(&Client::Cli, &req).unwrap();
        assert_eq!(done.remaining, 3);
        assert_eq!(attempts.load(Ordering::Acquire), 1);
        sim.shutdown();
    }
}

#[test]
fn equal_uris_after_an_intervening_edit_do_not_satisfy_a_stale_revision() {
    let (sim, surface, attempts) = setup(true, Fault::None);
    let host = sim.player("Alpha").unwrap().ip;
    let mut req = request(2, &["b"]);
    req.expected_revision = surface
        .queue(&Client::Cli, "Beta", 0, 100)
        .unwrap()
        .revision;
    let before = current(&sim);
    insert_other(&sim.lan(), host).unwrap();
    let updated = read(&sim.lan(), host).unwrap();
    soap::remove_track_range_from_queue(&sim.lan(), host, updated.revision, 5, 1).unwrap();
    assert_eq!(current(&sim), before);
    assert!(surface.queue_remove(&Client::Cli, &req).is_err());
    assert_eq!(attempts.load(Ordering::Acquire), 0);
    assert_eq!(current(&sim), before);
    sim.shutdown();
}

#[test]
fn old_removal_requests_decode_without_a_revision() {
    let decoded: QueueRemoveRequest = serde_json::from_value(serde_json::json!({
        "zone": "Beta", "position": 2,
        "expected_uris": ["x-file-cifs://example.invalid/b.flac"]
    }))
    .unwrap();
    assert_eq!(decoded, request(2, &["b"]));
    assert!(
        serde_json::to_value(decoded)
            .unwrap()
            .get("expected_revision")
            .is_none()
    );
}
