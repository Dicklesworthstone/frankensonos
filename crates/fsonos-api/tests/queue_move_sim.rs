//! Identity-bound moves through Surface and real localhost S1/S2 SOAP.

use fsonos_api::surface::queue::QueueMoveRequest;
use fsonos_api::{ErrorCode, Surface};
use fsonos_core::HouseholdState;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::{ActionFilter, MemStore};
use fsonos_proto::content::{QUEUE, browse_snapshot};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Copy, Default)]
enum Fault {
    #[default]
    None,
    Lost,
    Malformed,
    NotSent,
    EditBefore,
    EditAfter,
    ReadAfter,
}

struct Wire {
    inner: SimLan,
    fault: Mutex<Fault>,
    moves: AtomicUsize,
    broken_read: AtomicBool,
}

fn lost() -> ProtoError {
    ProtoError::Network {
        target: "test-player".into(),
        detail: "reply lost".into(),
    }
}

fn append(lan: &SimLan, host: IpAddr, name: &str) {
    soap::enqueue_uri(lan, host, &uri(name), "", 0, false).unwrap();
}

fn uri(name: &str) -> String {
    format!("x-file-cifs://example.invalid/{name}.flac")
}

impl Transport for Wire {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if self.broken_read.load(Ordering::Acquire) && action.ends_with("#Browse\"") {
            return Err(lost());
        }
        if !action.ends_with("#ReorderTracksInQueue\"") {
            return self.inner.soap_post(host, path, action, body);
        }
        self.moves.fetch_add(1, Ordering::AcqRel);
        let fault = *self.fault.lock().unwrap();
        if matches!(fault, Fault::NotSent) {
            return Err(lost());
        }
        if matches!(fault, Fault::EditBefore) {
            append(&self.inner, host, "external");
        }
        let reply = self.inner.soap_post(host, path, action, body)?;
        match fault {
            Fault::Lost => Err(lost()),
            Fault::Malformed => Ok("invalid SOAP".into()),
            Fault::ReadAfter => {
                self.broken_read.store(true, Ordering::Release);
                Err(lost())
            }
            Fault::EditAfter => {
                append(&self.inner, host, "external");
                Err(lost())
            }
            _ => Ok(reply),
        }
    }
}

struct Fixture {
    sim: SimHandle,
    surface: Surface,
    wire: Arc<Wire>,
}

impl Fixture {
    fn new(s2: bool, names: &[&str]) -> Self {
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
        let host = sim.player("Alpha").unwrap().ip;
        for name in names {
            append(&lan, host, name);
        }
        let source = format!("x-rincon-queue:{}#0", sim.player("Alpha").unwrap().uuid);
        soap::set_av_transport_uri(&lan, host, &source, "").unwrap();
        soap::seek_track(&lan, host, 2).unwrap();
        soap::play(&lan, host).unwrap();
        sim.advance(Duration::from_secs(13));
        let join = format!("x-rincon:{}", sim.player("Alpha").unwrap().uuid);
        soap::set_av_transport_uri(&lan, sim.player("Beta").unwrap().ip, &join, "").unwrap();
        let mut household = HouseholdState::default();
        household.apply_topology(&get_zone_group_state(&lan, host).unwrap());
        let wire = Arc::new(Wire {
            inner: lan,
            fault: Mutex::new(Fault::None),
            moves: AtomicUsize::new(0),
            broken_read: AtomicBool::new(false),
        });
        let surface = Surface::new(
            Box::new(Arc::clone(&wire)),
            Box::new(move |_| Ok(vec![household.clone()])),
            Policy::default(),
            Box::new(SystemClock),
        )
        .with_action_log(Box::new(MemStore::default()), "http");
        Self {
            sim,
            surface,
            wire,
        }
    }

    fn request(&self, position: u32, count: usize, insert_before: u32) -> QueueMoveRequest {
        let page = self.surface.queue(&Client::Cli, "Beta", 0, 100).unwrap();
        let start = usize::try_from(position - 1).unwrap();
        QueueMoveRequest {
            zone: "Beta".into(),
            position,
            insert_before,
            expected_uris: page.items[start..start + count]
                .iter()
                .map(|i| i.uri.clone().unwrap())
                .collect(),
            expected_revision: page.revision.unwrap(),
        }
    }

    fn current(&self) -> Vec<String> {
        let host = self.sim.player("Alpha").unwrap().ip;
        browse_snapshot(&self.sim.lan(), host, QUEUE)
            .unwrap()
            .objects
            .into_iter()
            .map(|o| o.res.unwrap().uri)
            .collect()
    }

    fn fault(&self, fault: Fault) {
        *self.wire.fault.lock().unwrap() = fault;
    }

    fn attempts(&self) -> usize {
        self.wire.moves.load(Ordering::Acquire)
    }
}

#[test]
fn moves_a_block_through_a_group_member_without_touching_playback_on_s1_and_s2() {
    for s2 in [false, true] {
        let f = Fixture::new(s2, &["a", "b", "c", "d", "e"]);
        let req = f.request(2, 2, 6);
        let host = f.sim.player("Alpha").unwrap().ip;
        let before = soap::get_position_info(&f.sim.lan(), host).unwrap();
        let done = f.surface.queue_move(&Client::LoopbackHttp, &req).unwrap();
        assert_eq!((done.position, done.count, done.total), (4, 2, 5));
        assert!(done.changed);
        assert!(!done.recovered);
        assert!(!done.undoable);
        assert_eq!(f.current(), ["a", "d", "e", "b", "c"].map(uri));
        let after = soap::get_position_info(&f.sim.lan(), host).unwrap();
        assert_eq!(after.uri, before.uri);
        assert_eq!(after.position_secs, before.position_secs);
        assert_eq!(after.track, 4);
        assert_ne!(done.revision, req.expected_revision);
        assert_eq!(f.attempts(), 1);
        let log = f
            .surface
            .recent_actions(&Client::Cli, &ActionFilter::default())
            .unwrap();
        assert_eq!(log.len(), 1);
        assert!(log[0].action.intent.starts_with("move_in_queue"));
        assert!(log[0].action.before_state.is_none());
    }
}

#[test]
fn noop_destinations_never_issue_a_native_write() {
    let f = Fixture::new(true, &["a", "b", "c", "d", "e"]);
    for before in 2..=4 {
        let req = f.request(2, 2, before);
        let done = f.surface.queue_move(&Client::Cli, &req).unwrap();
        assert!(!done.changed);
        assert_eq!(done.position, 2);
        assert_eq!(done.revision, req.expected_revision);
    }
    assert_eq!(f.attempts(), 0);
}

#[test]
fn stale_source_destination_revision_and_invalid_ranges_never_move() {
    let f = Fixture::new(true, &["a", "b", "c", "d", "e"]);
    let good = f.request(2, 2, 6);
    let mut bad = good.clone();
    bad.expected_revision = "different-coordinator".into();
    assert!(f.surface.queue_move(&Client::Cli, &bad).is_err());
    bad = good.clone();
    bad.expected_uris[0] = uri("other");
    assert!(f.surface.queue_move(&Client::Cli, &bad).is_err());
    bad = good.clone();
    bad.insert_before = 7;
    assert!(f.surface.queue_move(&Client::Cli, &bad).is_err());
    append(&f.sim.lan(), f.sim.player("Alpha").unwrap().ip, "external");
    assert!(f.surface.queue_move(&Client::Cli, &good).is_err());
    assert_eq!(f.attempts(), 0);
}

#[test]
fn lost_or_malformed_replies_are_confirmed_without_a_second_write() {
    for fault in [Fault::Lost, Fault::Malformed] {
        let f = Fixture::new(true, &["a", "b", "c", "d", "e"]);
        let req = f.request(2, 2, 6);
        f.fault(fault);
        let done = f.surface.queue_move(&Client::Cli, &req).unwrap();
        assert!(done.recovered);
        assert_eq!(done.position, 4);
        assert_eq!(f.attempts(), 1);
        assert_eq!(f.current(), ["a", "d", "e", "b", "c"].map(uri));
    }
}

#[test]
fn unconfirmed_or_concurrently_changed_results_are_nonretryable() {
    for fault in [
        Fault::NotSent,
        Fault::EditBefore,
        Fault::EditAfter,
        Fault::ReadAfter,
    ] {
        let f = Fixture::new(true, &["a", "b", "c", "d", "e"]);
        let req = f.request(2, 2, 6);
        f.fault(fault);
        let error = f.surface.queue_move(&Client::Cli, &req).unwrap_err();
        assert_eq!(error.code, ErrorCode::UpnpFault);
        assert!(!error.body().retryable);
        assert_eq!(f.attempts(), 1);
    }
}

#[test]
fn native_refusal_and_ignored_success_do_not_claim_a_move() {
    for ignored in [false, true] {
        let f = Fixture::new(true, &["a", "b", "c", "d", "e"]);
        let req = f.request(2, 2, 6);
        if ignored {
            f.sim
                .ignore_action("Alpha", "ReorderTracksInQueue")
                .unwrap();
        } else {
            f.sim
                .upnp_fault("Alpha", "ReorderTracksInQueue", 402)
                .unwrap();
        }
        assert!(f.surface.queue_move(&Client::Cli, &req).is_err());
        assert_eq!(f.current(), ["a", "b", "c", "d", "e"].map(uri));
        assert_eq!(f.attempts(), 1);
    }
}

#[test]
fn identical_duplicates_with_a_lost_reply_are_not_falsely_confirmed() {
    let f = Fixture::new(true, &["a", "a"]);
    let req = f.request(1, 1, 3);
    f.fault(Fault::Lost);
    let error = f.surface.queue_move(&Client::Cli, &req).unwrap_err();
    assert!(error.detail.contains("ambiguous"));
    assert!(!error.body().retryable);
    assert_eq!(f.attempts(), 1);
}

#[test]
fn unknown_callers_are_refused_and_do_not_move_even_with_a_valid_selection() {
    let f = Fixture::new(true, &["a", "b", "c", "d", "e"]);
    let req = f.request(2, 2, 6);
    let log_len = f.sim.soap_log().len();
    let error = f.surface.queue_move(&Client::Unknown, &req).unwrap_err();
    assert_eq!(error.code, ErrorCode::PolicyDenied);
    assert_eq!(f.sim.soap_log().len(), log_len);
    assert_eq!(f.attempts(), 0);
}

#[test]
fn coordinator_changes_invalidate_the_old_token_but_fresh_reads_can_move() {
    let f = Fixture::new(true, &["a", "b", "c", "d", "e"]);
    let req = f.request(2, 2, 6);
    f.sim.reelect_coordinator("Alpha").unwrap();
    assert!(f.surface.queue_move(&Client::Cli, &req).is_err());
    assert_eq!(f.attempts(), 0);
    let req = f.request(2, 2, 6);
    let done = f.surface.queue_move(&Client::Cli, &req).unwrap();
    assert_eq!(done.zone, "Beta");
    assert_eq!(done.position, 4);
    assert_eq!(f.attempts(), 1);
}
