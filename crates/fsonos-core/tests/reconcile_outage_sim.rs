//! A household must recover after several absent surveys, even when it no
//! longer advertises. Discovery is controlled; all reads and GENA use sockets.

use fsonos_core::events::{Service, Subscriber, Want};
use fsonos_core::reconcile::{HOUSELESS_RETRY, Health, Reconciler};
use fsonos_core::{control, resolve_room};
use fsonos_proto::gena::Subscription;
use fsonos_proto::net::{EventSink, Lan};
use fsonos_proto::ssdp::{Advert, description_url};
use fsonos_proto::topology::host_of_location;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use std::cell::RefCell;
use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

struct FilteredLan {
    inner: Lan,
    visible: RefCell<Vec<IpAddr>>,
    gets: RefCell<Vec<IpAddr>>,
}

impl Transport for FilteredLan {
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

impl Subscriber for FilteredLan {
    fn subscribe(&self, want: &Want, callback: &str) -> Result<Subscription, ProtoError> {
        Subscriber::subscribe(&self.inner, want, callback)
    }

    fn renew(&self, want: &Want, sid: &str) -> Result<Subscription, ProtoError> {
        Subscriber::renew(&self.inner, want, sid)
    }

    fn unsubscribe(&self, want: &Want, sid: &str) -> Result<(), ProtoError> {
        Subscriber::unsubscribe(&self.inner, want, sid)
    }
}

fn fixture() -> (SimHandle, FilteredLan, EventSink) {
    let sim = SimHousehold::builder()
        .s1([
            SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1),
            SimPlayerSpec::new("Office", SimModel::Play5Gen1),
        ])
        .s2([SimPlayerSpec::new("Living Room", SimModel::One)])
        .spawn()
        .unwrap();
    let inner = Lan::start()
        .unwrap()
        .with_routes(sim.players().iter().map(|p| (p.ip, p.addr)).collect());
    let local = inner
        .local_address_toward(sim.player("Kitchen").unwrap().ip)
        .unwrap();
    let sink = EventSink::start(SocketAddr::new(local, 0)).unwrap();
    let visible = sim.players().iter().map(|p| p.ip).collect();
    let lan = FilteredLan {
        inner,
        visible: RefCell::new(visible),
        gets: RefCell::new(Vec::new()),
    };
    (sim, lan, sink)
}

fn once(rec: &mut Reconciler, lan: &FilteredLan, sink: &EventSink, now: Instant) {
    lan.gets.borrow_mut().clear();
    let report = rec
        .refresh(lan, &[], |s: Service| sink.callback_url(s.tag()), now)
        .unwrap();
    assert!(
        report.events.failed.is_empty(),
        "{:?}",
        report.events.failed
    );
    let gets = lan.gets.borrow();
    assert_eq!(
        gets.len(),
        gets.iter().collect::<HashSet<_>>().len(),
        "no repeated address probes in a survey: {gets:?}"
    );
}

#[test]
fn a_silent_household_returns_after_multiple_offline_surveys() {
    let (sim, lan, sink) = fixture();
    let now = Instant::now();
    let mut rec = Reconciler::new(Duration::from_mins(5), Duration::from_mins(30), now);
    once(&mut rec, &lan, &sink, now);
    let living = resolve_room(&rec.households, "Living Room")
        .unwrap()
        .player
        .id
        .clone();
    let all = rec.subscriptions.len();
    sim.set_offline("Living Room", true).unwrap();
    *lan.visible.borrow_mut() = vec![sim.player("Kitchen").unwrap().ip];
    for second in 1..=3 {
        once(&mut rec, &lan, &sink, now + Duration::from_secs(second));
        assert_eq!(rec.households.len(), 1);
        assert!(resolve_room(&rec.households, "Living Room").is_err());
        assert_eq!(rec.health.of(&living).unwrap().health, Health::Offline);
        assert!(rec.subscriptions.len() < all);
    }
    sim.set_offline("Living Room", false).unwrap();
    once(&mut rec, &lan, &sink, now + Duration::from_secs(4));
    assert_eq!(rec.households.len(), 2);
    assert_eq!(rec.health.of(&living).unwrap().health, Health::Healthy);
    assert_eq!(rec.subscriptions.len(), all);
    assert_eq!(
        control::set_volume(&lan, &rec.households, &living, 23).unwrap(),
        23
    );
    assert_eq!(control::volume(&lan, &rec.households, &living).unwrap(), 23);
    rec.subscriptions.unsubscribe_all(&lan);
}

#[test]
fn a_player_lost_before_its_last_peer_is_still_a_rediscovery_candidate() {
    let (sim, lan, sink) = fixture();
    let now = Instant::now();
    let mut rec = Reconciler::new(Duration::from_mins(5), Duration::from_mins(30), now);
    once(&mut rec, &lan, &sink, now);
    let office = resolve_room(&rec.households, "Office")
        .unwrap()
        .player
        .id
        .clone();
    lan.visible.borrow_mut().clear();
    sim.set_offline("Office", true).unwrap();
    once(&mut rec, &lan, &sink, now + Duration::from_secs(1));
    assert!(resolve_room(&rec.households, "Office").is_err());
    sim.set_offline("Kitchen", true).unwrap();
    for second in 2..=4 {
        once(&mut rec, &lan, &sink, now + Duration::from_secs(second));
        assert_eq!(rec.households.len(), 1);
        assert!(resolve_room(&rec.households, "Kitchen").is_err());
    }
    sim.set_offline("Office", false).unwrap();
    once(&mut rec, &lan, &sink, now + Duration::from_secs(5));
    assert_eq!(rec.households.len(), 2);
    assert!(resolve_room(&rec.households, "Kitchen").is_err());
    assert_eq!(rec.health.of(&office).unwrap().health, Health::Healthy);
    assert_eq!(
        control::set_volume(&lan, &rec.households, &office, 19).unwrap(),
        19
    );
    rec.subscriptions.unsubscribe_all(&lan);
}

#[test]
fn complete_discovery_skips_remembered_offline_peers_after_an_outage() {
    let (sim, lan, sink) = fixture();
    let now = Instant::now();
    let mut rec = Reconciler::new(Duration::from_mins(5), Duration::from_mins(30), now);
    once(&mut rec, &lan, &sink, now);
    let kitchen = sim.player("Kitchen").unwrap().ip;
    let living = sim.player("Living Room").unwrap().ip;
    sim.set_offline("Office", true).unwrap();
    *lan.visible.borrow_mut() = vec![kitchen, living];
    once(&mut rec, &lan, &sink, now + Duration::from_secs(1));
    assert_eq!(*lan.gets.borrow(), [kitchen, living]);
    assert!(resolve_room(&rec.households, "Office").is_err());
    rec.subscriptions.unsubscribe_all(&lan);
}

/// A daemon that has not found the house yet (the speaker LAN was down when
/// it started, say) keeps looking every half minute instead of backing off
/// toward half an hour, and finds the house on the first survey after it
/// answers again.
#[test]
fn a_model_without_the_house_keeps_looking_every_half_minute() {
    let (_sim, lan, sink) = fixture();
    let now = Instant::now();
    let mut rec = Reconciler::new(Duration::from_mins(5), Duration::from_mins(30), now);
    let visible = lan.visible.replace(Vec::new());
    let mut at = now;
    for attempt in 0..6 {
        let failed = rec.refresh(&lan, &[], |s: Service| sink.callback_url(s.tag()), at);
        assert!(failed.is_err(), "attempt {attempt}: nothing answers");
        let next = rec.schedule.next_at();
        assert!(
            next <= at + HOUSELESS_RETRY,
            "attempt {attempt}: next survey in {:?}",
            next - at
        );
        at = next;
    }
    *lan.visible.borrow_mut() = visible;
    once(&mut rec, &lan, &sink, at);
    assert_eq!(rec.households.len(), 2);
}
