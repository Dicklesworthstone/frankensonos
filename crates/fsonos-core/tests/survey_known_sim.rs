//! Partial or blocked discovery must not hide reachable known households.
//! Only discovery is filtered; description, SOAP and GENA use real sockets.

use fsonos_core::events::{Service, Subscriber, Want};
use fsonos_core::heal::{self, Recovery};
use fsonos_core::inventory::{survey, survey_known};
use fsonos_core::reconcile::{Health, Reconciler};
use fsonos_core::{HouseholdState, control, resolve_room};
use fsonos_proto::gena::Subscription;
use fsonos_proto::net::{EventSink, Lan};
use fsonos_proto::ssdp::{Advert, description_url};
use fsonos_proto::topology::host_of_location;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use fsonos_types::HouseholdId;
use std::cell::{Cell, RefCell};
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

struct Filtered<T> {
    inner: T,
    visible: RefCell<Vec<IpAddr>>,
    gets: RefCell<Vec<IpAddr>>,
    searches: Cell<usize>,
    fail_discovery: Cell<bool>,
}

impl<T> Filtered<T> {
    fn new(inner: T, visible: Vec<IpAddr>) -> Self {
        Self {
            inner,
            visible: RefCell::new(visible),
            gets: RefCell::new(Vec::new()),
            searches: Cell::new(0),
            fail_discovery: Cell::new(false),
        }
    }
}

impl<T: Transport> Transport for Filtered<T> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        self.inner.soap_post(host, path, action, body)
    }

    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        self.gets.borrow_mut().push(host_of_location(url).unwrap());
        self.inner.http_get(url)
    }

    fn ssdp_search(&self, _: u8, _: Duration) -> Result<Vec<Advert>, ProtoError> {
        self.searches.set(self.searches.get() + 1);
        if self.fail_discovery.get() {
            return Err(ProtoError::Network {
                target: "discovery".into(),
                detail: "multicast unavailable".into(),
            });
        }
        Ok(self
            .visible
            .borrow()
            .iter()
            .map(|&ip| Advert {
                location: description_url(ip),
                st: "urn:schemas-upnp-org:device:ZonePlayer:1".into(),
                usn: None,
                household: None,
                boot_seq: None,
            })
            .collect())
    }
}

impl<T: Subscriber> Subscriber for Filtered<T> {
    fn subscribe(&self, want: &Want, callback: &str) -> Result<Subscription, ProtoError> {
        self.inner.subscribe(want, callback)
    }

    fn renew(&self, want: &Want, sid: &str) -> Result<Subscription, ProtoError> {
        self.inner.renew(want, sid)
    }

    fn unsubscribe(&self, want: &Want, sid: &str) -> Result<(), ProtoError> {
        self.inner.unsubscribe(want, sid)
    }
}

fn sim() -> SimHandle {
    SimHousehold::builder()
        .s1([
            SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1),
            SimPlayerSpec::new("Office", SimModel::Play5Gen1),
        ])
        .s2([SimPlayerSpec::new("Living Room", SimModel::One)])
        .spawn()
        .unwrap()
}

fn known(sim: &SimHandle) -> Vec<HouseholdState> {
    let seeds: Vec<_> = sim.players().iter().map(|p| p.ip).collect();
    let mut houses = survey(&sim.lan(), &seeds, Duration::ZERO)
        .unwrap()
        .households;
    assert_eq!(houses.len(), 2);
    for (index, house) in houses.iter_mut().enumerate() {
        house.id = Some(HouseholdId(format!("synthetic-household-{index}")));
    }
    houses
}

#[test]
fn partial_discovery_probes_only_the_missing_household_and_retains_its_identity() {
    let sim = sim();
    let known = known(&sim);
    let kitchen = sim.player("Kitchen").unwrap().ip;
    let living = sim.player("Living Room").unwrap().ip;
    let lan = Filtered::new(sim.lan(), vec![kitchen]);

    let found = survey_known(&lan, &[], &known, Duration::ZERO).unwrap();

    assert_eq!(found.households.len(), 2);
    assert_eq!(found.households[0].id, known[0].id);
    assert_eq!(found.households[1].id, known[1].id);
    assert_eq!(*lan.gets.borrow(), [kitchen, living]);
    assert_eq!(lan.searches.get(), 1, "no second discovery window");
    assert!(found.unreachable.is_empty());
}

#[test]
fn complete_discovery_never_probes_extra_remembered_addresses() {
    let sim = sim();
    let known = known(&sim);
    let kitchen = sim.player("Kitchen").unwrap().ip;
    let living = sim.player("Living Room").unwrap().ip;
    let lan = Filtered::new(sim.lan(), vec![kitchen, living]);

    let found = survey_known(&lan, &[], &known, Duration::ZERO).unwrap();

    assert_eq!(found.households.len(), 2);
    assert_eq!(*lan.gets.borrow(), [kitchen, living]);
    assert_eq!(lan.searches.get(), 1);
}

#[test]
fn blocked_multicast_recovers_both_households_without_another_search() {
    let sim = sim();
    let known = known(&sim);
    let lan = Filtered::new(sim.lan(), Vec::new());
    lan.fail_discovery.set(true);

    let found = survey_known(&lan, &[], &known, Duration::ZERO).unwrap();

    assert_eq!(found.households.len(), 2);
    assert_eq!(found.boot_seqs.len(), 3);
    assert!(found.ssdp_error.unwrap().contains("multicast unavailable"));
    assert_eq!(lan.gets.borrow().len(), 3);
    assert_eq!(lan.searches.get(), 1);
    for player in sim.players() {
        assert_eq!(
            lan.gets
                .borrow()
                .iter()
                .filter(|&&ip| ip == player.ip)
                .count(),
            1,
            "each remembered address is tried only once"
        );
    }
}

#[test]
fn a_missing_household_is_not_resurrected_when_its_cached_address_fails() {
    let sim = sim();
    let known = known(&sim);
    let kitchen = sim.player("Kitchen").unwrap().ip;
    let living = sim.player("Living Room").unwrap().ip;
    sim.set_offline("Living Room", true).unwrap();
    let lan = Filtered::new(sim.lan(), vec![kitchen]);

    let found = survey_known(&lan, &[], &known, Duration::ZERO).unwrap();

    assert_eq!(found.households.len(), 1);
    assert!(resolve_room(&found.households, "Living Room").is_err());
    assert!(
        found
            .unreachable
            .iter()
            .any(|(ip, _)| ip.parse::<IpAddr>().ok() == Some(living))
    );
    assert_eq!(*lan.gets.borrow(), [kitchen, living]);
}

#[test]
fn live_topology_is_authoritative_for_a_vanished_member() {
    let sim = sim();
    let known = known(&sim);
    let kitchen = sim.player("Kitchen").unwrap().ip;
    let living = sim.player("Living Room").unwrap().ip;
    let office = resolve_room(&known, "Office").unwrap().player.id.clone();
    sim.set_offline("Office", true).unwrap();
    let lan = Filtered::new(sim.lan(), vec![kitchen, living]);

    let found = survey_known(&lan, &[], &known, Duration::ZERO).unwrap();

    assert_eq!(found.households.len(), 2);
    assert!(resolve_room(&found.households, "Office").is_err());
    assert!(found.vanished.iter().any(|v| v.uuid == office));
    assert_eq!(*lan.gets.borrow(), [kitchen, living]);
}

#[test]
fn failed_discovery_and_failed_fallback_preserve_the_original_error() {
    let sim = sim();
    let known = known(&sim);
    for room in ["Kitchen", "Office", "Living Room"] {
        sim.set_offline(room, true).unwrap();
    }
    let lan = Filtered::new(sim.lan(), Vec::new());
    lan.fail_discovery.set(true);

    let error = survey_known(&lan, &[], &known, Duration::ZERO).unwrap_err();

    assert!(error.to_string().contains("multicast unavailable"));
    assert_eq!(lan.searches.get(), 1);
    assert_eq!(lan.gets.borrow().len(), 3);
}

#[test]
fn known_addresses_already_tried_as_seeds_are_not_retried() {
    let sim = sim();
    let known = known(&sim);
    let kitchen = sim.player("Kitchen").unwrap().ip;
    let living = sim.player("Living Room").unwrap().ip;
    sim.set_offline("Living Room", true).unwrap();
    let lan = Filtered::new(sim.lan(), vec![kitchen]);

    let found = survey_known(&lan, &[living], &known, Duration::ZERO).unwrap();

    assert_eq!(found.households.len(), 1);
    assert_eq!(*lan.gets.borrow(), [kitchen, living]);
    assert_eq!(lan.searches.get(), 1);
}

#[test]
fn address_recovery_uses_a_known_peer_when_multicast_is_blocked() {
    let mut sim = sim();
    let mut houses = known(&sim);
    let kitchen = resolve_room(&houses, "Kitchen").unwrap().player.id.clone();
    let old = sim.player("Kitchen").unwrap().ip;
    let office = sim.player("Office").unwrap().ip;
    let other = houses[1].clone();
    let new = sim.change_address("Kitchen").unwrap();
    let lan = Filtered::new(sim.lan(), Vec::new());
    let attempts = Cell::new(0);

    let (level, recovery) = heal::readdressing(&lan, &[], &mut houses, &kitchen, |hs| {
        attempts.set(attempts.get() + 1);
        control::set_volume(&lan, hs, &kitchen, 23)
    })
    .unwrap();

    assert_eq!(level, 23);
    assert_eq!(control::volume(&lan, &houses, &kitchen).unwrap(), 23);
    assert_eq!(attempts.get(), 2);
    assert!(matches!(recovery, Recovery::Readdressed { to, .. } if to == new));
    assert_eq!(houses[1], other);
    assert_eq!(
        houses[0].id,
        Some(HouseholdId("synthetic-household-0".into()))
    );
    assert_eq!(*lan.gets.borrow(), [old, office]);
    assert_eq!(lan.searches.get(), 1);
}

#[test]
fn daemon_refresh_keeps_subscriptions_when_discovery_goes_silent() {
    let sim = sim();
    let kitchen = sim.player("Kitchen").unwrap().ip;
    let living = sim.player("Living Room").unwrap().ip;
    let inner = Lan::start()
        .unwrap()
        .with_routes(sim.players().iter().map(|p| (p.ip, p.addr)).collect());
    let local = inner.local_address_toward(kitchen).unwrap();
    let sink = EventSink::start(SocketAddr::new(local, 0)).unwrap();
    let lan = Filtered::new(inner, vec![kitchen, living]);
    let callback = |s: Service| sink.callback_url(s.tag());
    let now = Instant::now();
    let mut rec = Reconciler::new(Duration::from_mins(5), Duration::from_mins(30), now);
    let first = rec.refresh(&lan, &[], callback, now).unwrap();
    assert_eq!((first.households, first.players), (2, 3));
    assert!(first.events.failed.is_empty(), "{:?}", first.events.failed);
    let subscriptions = rec.subscriptions.len();
    lan.visible.borrow_mut().clear();
    lan.gets.borrow_mut().clear();

    let next = rec
        .refresh(&lan, &[], callback, now + Duration::from_secs(1))
        .unwrap();

    assert_eq!((next.households, next.players), (2, 3));
    assert!(next.missing.is_empty());
    assert!(next.events.failed.is_empty(), "{:?}", next.events.failed);
    assert_eq!(next.events.dropped, 0);
    assert_eq!(rec.subscriptions.len(), subscriptions);
    assert_eq!(lan.gets.borrow().len(), 3);
    for player in rec.households.iter().flat_map(|h| &h.players) {
        assert_eq!(rec.health.of(&player.id).unwrap().health, Health::Healthy);
    }
    rec.subscriptions.unsubscribe_all(&lan);
}
