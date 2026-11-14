//! Incoherent topology NOTIFYs must not erase a household or its leases.
//! Exercise admission and recovery against both real S1 and S2 simulator streams.

use fsonos_core::HouseholdState;
use fsonos_core::events::{RETRY, Service, Subscriber, Want, wanted};
use fsonos_core::playback::Playback;
use fsonos_core::reconcile::{Health, NotifyReport, Reconciler};
use fsonos_proto::ProtoError;
use fsonos_proto::gena::{Notify, Subscription};
use fsonos_proto::net::{EventSink, Lan};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use fsonos_types::PlayerId;
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

const EMPTY: &str = "<ZoneGroupState><ZoneGroups/></ZoneGroupState>";

struct Fixture {
    sim: SimHandle,
    lan: Lan,
    sink: EventSink,
    rec: Reconciler,
    playback: Playback,
    topology: HashMap<PlayerId, Notify>,
    now: Instant,
}

impl Fixture {
    fn new() -> Self {
        let sim = SimHousehold::builder()
            .s1([
                SimPlayerSpec::pair("Legacy", SimModel::Play5Gen1),
                SimPlayerSpec::new("Legacy peer", SimModel::Play5Gen1),
            ])
            .s2([
                SimPlayerSpec::pair("Modern", SimModel::One),
                SimPlayerSpec::new("Modern peer", SimModel::One),
            ])
            .spawn()
            .unwrap();
        let lan = Lan::start()
            .unwrap()
            .with_routes(sim.players().iter().map(|p| (p.ip, p.addr)).collect());
        let now = Instant::now();
        let interval = Duration::from_mins(5);
        let mut rec = Reconciler::new(interval, interval, now);
        for name in ["Legacy", "Modern"] {
            let host = sim.player(name).unwrap().ip;
            let mut house = HouseholdState::default();
            house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
            rec.households.push(house);
        }
        rec.schedule.succeeded(now);
        let local = lan
            .local_address_toward(sim.player("Legacy").unwrap().ip)
            .unwrap();
        let sink = EventSink::start(SocketAddr::new(local, 0)).unwrap();
        let mut fixture = Self {
            sim,
            lan,
            sink,
            rec,
            playback: Playback::default(),
            topology: HashMap::new(),
            now,
        };
        let desired = wanted(&fixture.rec.households);
        let report = fixture.rec.subscriptions.sync(
            &fixture.lan,
            &desired,
            |s| fixture.sink.callback_url(s.tag()),
            now,
        );
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert_eq!(report.subscribed, desired.len());
        let mut seen = HashSet::new();
        while seen.len() < desired.len() {
            let notify = fixture.receive();
            let (player, service) = fixture.rec.subscriptions.route(&notify).unwrap();
            seen.insert((player.clone(), service));
            assert!(fixture.accept(&notify).is_some());
        }
        assert_eq!(fixture.topology.len(), 2);
        assert_eq!(fixture.rec.households.iter().map(|h| h.players.len()).sum::<usize>(), 6);
        fixture
    }

    fn receive(&self) -> Notify {
        self.sink
            .recv_timeout(Duration::from_secs(5))
            .expect("simulator did not deliver its NOTIFY")
    }

    fn accept(&mut self, notify: &Notify) -> Option<NotifyReport> {
        let route = self
            .rec
            .subscriptions
            .route(notify)
            .map(|(p, s)| (p.clone(), s));
        let report = self
            .rec
            .on_notify(
                &self.lan,
                &mut self.playback,
                notify,
                |s| self.sink.callback_url(s.tag()),
                self.now,
            )
            .unwrap();
        if let Some((player, Service::ZoneGroupTopology)) = route
            && report.is_some()
        {
            self.topology.insert(player, notify.clone());
        }
        report
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.rec.subscriptions.unsubscribe_all(&self.lan);
    }
}

fn with_document(basis: &Notify, document: &str) -> Notify {
    let mut notify = basis.clone();
    notify.seq += 1;
    notify
        .properties
        .iter_mut()
        .find(|(name, _)| name == "ZoneGroupState")
        .unwrap()
        .1 = document.into();
    notify
}

#[test]
fn s1_and_s2_recover_incomplete_events_without_losing_rooms_or_subscriptions() {
    let mut fixture = Fixture::new();
    let originals: Vec<_> = fixture.topology.clone().into_iter().collect();
    for (sender, mut current) in originals {
        let foreign = fixture
            .topology
            .iter()
            .find(|(player, _)| **player != sender)
            .unwrap()
            .1
            .property("ZoneGroupState")
            .unwrap()
            .to_string();
        for document in [EMPTY, foreign.as_str()] {
            let households = fixture.rec.households.clone();
            let states: Vec<_> = households
                .iter()
                .flat_map(|house| &house.players)
                .map(|player| {
                    (player.id.clone(), fixture.playback.of(&player.id).unwrap().clone())
                })
                .collect();
            let leases = fixture.rec.subscriptions.len();
            let invalid = with_document(&current, document);
            assert!(
                fixture
                    .rec
                    .on_notify(
                        &fixture.lan,
                        &mut fixture.playback,
                        &invalid,
                        |s| fixture.sink.callback_url(s.tag()),
                        fixture.now,
                    )
                    .is_err()
            );
            assert_eq!(fixture.rec.households, households);
            assert_eq!(fixture.rec.subscriptions.len(), leases);
            for (player, state) in states {
                assert_eq!(fixture.playback.of(&player), Some(&state));
                assert_ne!(fixture.rec.health.of(&player).unwrap().health, Health::Offline);
            }

            // Recovery requests a real full-state event from a new SID.
            let fresh = fixture.receive();
            assert_eq!(fresh.seq, 0);
            assert_ne!(fresh.sid, current.sid);
            assert_eq!(
                fixture.rec.subscriptions.route(&fresh),
                Some((&sender, Service::ZoneGroupTopology))
            );
            let report = fixture.accept(&fresh).unwrap();
            assert!(report.gone.is_empty());
            assert!(report.rebooted.is_empty());
            assert_eq!(fixture.rec.households, households);
            assert!(fixture.rec.subscriptions.route(&invalid).is_none());
            current = fresh;
        }
    }
    assert_eq!(fixture.rec.schedule.next_at(), fixture.now + RETRY);

    // Coherent departures still apply: one peer really powers off in each
    // household, while both surviving stereo pairs retain their two renderers.
    for name in ["Legacy peer", "Modern peer"] {
        let peer = fixture
            .rec
            .households
            .iter()
            .flat_map(|house| &house.players)
            .find(|player| player.room_name == name)
            .unwrap()
            .id
            .clone();
        assert!(!fixture.topology.contains_key(&peer));
        let leases = fixture.rec.subscriptions.len();
        fixture.sim.set_offline(name, true).unwrap();
        let started = Instant::now();
        let mut gone = false;
        while !gone && started.elapsed() < Duration::from_secs(5) {
            if let Some(notify) = fixture.sink.recv_timeout(Duration::from_millis(200))
                && let Some(report) = fixture.accept(&notify)
            {
                gone = report.gone.contains(&peer);
            }
        }
        assert!(gone, "{name}'s coherent disappearance was not adopted");
        assert!(
            fixture
                .rec
                .households
                .iter()
                .all(|house| house.player(&peer).is_none())
        );
        assert_eq!(fixture.rec.health.of(&peer).unwrap().health, Health::Offline);
        assert!(fixture.rec.subscriptions.len() < leases);
    }
    assert!(fixture.rec.households.iter().all(|house| {
        house.players.len() == 2 && house.rooms.len() == 1 && house.rooms[0].players.len() == 2
    }));
}

struct RefuseSubscribe<'a> {
    lan: &'a Lan,
    attempts: Cell<usize>,
}

impl Subscriber for RefuseSubscribe<'_> {
    fn subscribe(&self, _: &Want, _: &str) -> Result<Subscription, ProtoError> {
        self.attempts.set(self.attempts.get() + 1);
        Err(ProtoError::NotWired("injected subscription outage"))
    }

    fn renew(&self, want: &Want, sid: &str) -> Result<Subscription, ProtoError> {
        Subscriber::renew(self.lan, want, sid)
    }

    fn unsubscribe(&self, want: &Want, sid: &str) -> Result<(), ProtoError> {
        Subscriber::unsubscribe(self.lan, want, sid)
    }
}

#[test]
fn rejected_topology_keeps_the_event_cursor_and_bounds_recovery_retries() {
    let mut fixture = Fixture::new();
    let before = fixture.rec.households.clone();
    let basis = fixture.topology.values().next().unwrap().clone();
    let invalid = with_document(&basis, EMPTY);
    let outage = RefuseSubscribe {
        lan: &fixture.lan,
        attempts: Cell::new(0),
    };
    for seconds in [0, 1] {
        assert!(
            fixture
                .rec
                .on_notify(
                    &outage,
                    &mut fixture.playback,
                    &invalid,
                    |s| fixture.sink.callback_url(s.tag()),
                    fixture.now + Duration::from_secs(seconds),
                )
                .is_err()
        );
        assert_eq!(fixture.rec.households, before);
        assert!(fixture.rec.subscriptions.route(&invalid).is_some());
        assert_eq!(fixture.rec.schedule.next_at(), fixture.now + RETRY);
        assert_eq!(fixture.rec.subscriptions.next_due(), Some(fixture.now + RETRY));
    }
    assert_eq!(outage.attempts.get(), 1, "bad events cannot bypass retry backoff");

    // A corrected document at the rejected SEQ remains usable on the old
    // lease when its replacement could not be established.
    let corrected = with_document(&basis, basis.property("ZoneGroupState").unwrap());
    let report = fixture
        .rec
        .on_notify(
            &outage,
            &mut fixture.playback,
            &corrected,
            |s| fixture.sink.callback_url(s.tag()),
            fixture.now + Duration::from_secs(2),
        )
        .unwrap()
        .unwrap();
    assert!(report.gone.is_empty());
    assert_eq!(fixture.rec.households, before);
    assert!(fixture.rec.subscriptions.route(&corrected).is_none());
    assert_eq!(outage.attempts.get(), 1);
}
