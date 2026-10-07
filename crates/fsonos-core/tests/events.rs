//! The event engine's bookkeeping with a fake subscriber: which subscriptions
//! the households need, reconciling the active set, renewal timing and the
//! resubscribe fallback, and NOTIFY routing.

use fsonos_core::HouseholdState;
use fsonos_core::events::{self, Service, Subscriber, Subscriptions, Want, wanted};
use fsonos_proto::ProtoError;
use fsonos_proto::gena::{Notify, Subscription};
use fsonos_proto::{soap, topology};
use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

const ZGS_S1: &str = include_str!("../../fsonos-proto/tests/fixtures/zgs_s1.xml");
const ZGS_S2: &str = include_str!("../../fsonos-proto/tests/fixtures/zgs_s2.xml");

fn household(body: &str) -> HouseholdState {
    let zgs = topology::parse_zone_group_state(
        soap::parse_response(body, "GetZoneGroupState")
            .unwrap()
            .require("ZoneGroupState")
            .unwrap(),
    )
    .unwrap();
    let mut st = HouseholdState::default();
    st.apply_topology(&zgs);
    st
}

/// Grants 300 s subscriptions with numbered SIDs; renewals of SIDs listed
/// in `stale` fail like a rebooted player's (412).
#[derive(Default)]
struct Fake {
    next: Cell<u32>,
    log: RefCell<Vec<String>>,
    stale: RefCell<Vec<String>>,
    /// Refuse this many SUBSCRIBEs before granting again.
    refuse: Cell<u32>,
}

impl Subscriber for Fake {
    fn subscribe(&self, want: &Want, callback_url: &str) -> Result<Subscription, ProtoError> {
        if self.refuse.get() > 0 {
            self.refuse.set(self.refuse.get() - 1);
            return Err(ProtoError::Network {
                target: "player".into(),
                detail: "SUBSCRIBE: HTTP 503".into(),
            });
        }
        self.next.set(self.next.get() + 1);
        let sid = format!("uuid:sub{}", self.next.get());
        self.log.borrow_mut().push(format!(
            "SUBSCRIBE {:?} {} -> {sid}",
            want.service, callback_url
        ));
        Ok(Subscription {
            sid,
            timeout_secs: 300,
        })
    }

    fn renew(&self, _want: &Want, sid: &str) -> Result<Subscription, ProtoError> {
        self.log.borrow_mut().push(format!("RENEW {sid}"));
        if self.stale.borrow().iter().any(|s| s == sid) {
            return Err(ProtoError::Network {
                target: "player".into(),
                detail: "SUBSCRIBE: HTTP 412".into(),
            });
        }
        Ok(Subscription {
            sid: sid.into(),
            timeout_secs: 300,
        })
    }

    fn unsubscribe(&self, _want: &Want, sid: &str) -> Result<(), ProtoError> {
        self.log.borrow_mut().push(format!("UNSUBSCRIBE {sid}"));
        Ok(())
    }
}

fn url(service: Service) -> String {
    format!("http://192.0.2.1:3400/{}", service.tag())
}

#[test]
fn every_household_gets_topology_once_and_each_service_where_it_belongs() {
    let houses = [household(ZGS_S1), household(ZGS_S2)];
    let w = wanted(&houses);
    for h in &houses {
        let mine = |s: Service| {
            w.iter()
                .filter(|x| x.service == s && h.player(&x.player).is_some())
                .count()
        };
        assert_eq!(mine(Service::ZoneGroupTopology), 1);
        assert_eq!(
            mine(Service::AvTransport),
            h.groups.len(),
            "one per coordinator"
        );
        assert_eq!(mine(Service::GroupRenderingControl), h.groups.len());
        assert_eq!(
            mine(Service::RenderingControl),
            h.players.len(),
            "one per renderer"
        );
    }
    assert!(w.iter().all(|x| {
        x.service != Service::AvTransport
            || houses
                .iter()
                .any(|h| h.groups.iter().any(|g| g.coordinator == x.player))
    }));
}

#[test]
fn sync_subscribes_what_is_missing_and_drops_what_is_gone() {
    let houses = [household(ZGS_S1)];
    let w = wanted(&houses);
    let fake = Fake::default();
    let mut subs = Subscriptions::default();
    let now = Instant::now();

    let first = subs.sync(&fake, &w, url, now);
    assert_eq!(first.subscribed, w.len());
    assert_eq!(subs.len(), w.len());
    assert!(
        fake.log.borrow()[0].contains("/ZoneGroupTopology"),
        "callback per service"
    );

    let again = subs.sync(&fake, &w, url, now);
    assert_eq!((again.subscribed, again.dropped), (0, 0), "already in sync");

    // A renderer leaves: its RenderingControl subscription goes.
    let fewer: Vec<Want> = w
        .iter()
        .filter(|x| {
            !(x.service == Service::RenderingControl && x.player == w.last().unwrap().player)
        })
        .cloned()
        .collect();
    let shrink = subs.sync(&fake, &fewer, url, now);
    assert_eq!(shrink.dropped, w.len() - fewer.len());
    assert!(fake.log.borrow().last().unwrap().starts_with("UNSUBSCRIBE"));
    assert_eq!(subs.len(), fewer.len());
}

#[test]
fn renewals_happen_halfway_and_a_stale_sid_is_resubscribed() {
    let houses = [household(ZGS_S2)];
    let w = wanted(&houses);
    let fake = Fake::default();
    let mut subs = Subscriptions::default();
    let t0 = Instant::now();
    subs.sync(&fake, &w, url, t0);
    assert_eq!(subs.next_due(), Some(t0 + Duration::from_secs(150)));

    let early = subs.renew_due(&fake, url, t0 + Duration::from_secs(149));
    assert_eq!(early.renewed + early.resubscribed, 0, "nothing due yet");

    // One player rebooted: its first subscription's renewal answers 412.
    fake.stale.borrow_mut().push("uuid:sub1".into());
    let due = subs.renew_due(&fake, url, t0 + Duration::from_secs(150));
    assert_eq!(due.resubscribed, 1);
    assert_eq!(due.renewed, w.len() - 1);
    assert!(due.failed.is_empty());
    assert_eq!(subs.len(), w.len());
    assert_eq!(subs.next_due(), Some(t0 + Duration::from_secs(300)));
}

#[test]
fn notifies_route_to_their_player_and_service_and_shutdown_unsubscribes() {
    let houses = [household(ZGS_S1)];
    let w = wanted(&houses);
    let fake = Fake::default();
    let mut subs = Subscriptions::default();
    subs.sync(&fake, &w, url, Instant::now());

    let n = |sid: &str| Notify {
        sid: sid.into(),
        seq: 0,
        path: "/".into(),
        properties: Vec::new(),
    };
    let (player, service) = subs.route(&n("uuid:sub1")).unwrap();
    assert_eq!((player, service), (&w[0].player, w[0].service));
    assert!(subs.route(&n("uuid:stray")).is_none());

    subs.unsubscribe_all(&fake);
    assert!(subs.is_empty());
    let unsubscribed = fake
        .log
        .borrow()
        .iter()
        .filter(|l| l.starts_with("UNSUBSCRIBE"))
        .count();
    assert_eq!(unsubscribed, w.len());
}

fn zgs(boot_a: u32, boot_b: u32) -> fsonos_proto::topology::ZoneGroupState {
    topology::parse_zone_group_state(&format!(
        "<ZoneGroupState><ZoneGroups><ZoneGroup Coordinator=\"RINCON_000E58A0000101400\" ID=\"g:1\">\
         <ZoneGroupMember UUID=\"RINCON_000E58A0000101400\" Location=\"http://192.0.2.10:1400/xml/device_description.xml\" \
         ZoneName=\"Den\" BootSeq=\"{boot_a}\"/>\
         <ZoneGroupMember UUID=\"RINCON_000E58A0000201400\" Location=\"http://192.0.2.11:1400/xml/device_description.xml\" \
         ZoneName=\"Study\" BootSeq=\"{boot_b}\"/></ZoneGroup></ZoneGroups></ZoneGroupState>"
    ))
    .unwrap()
}

#[test]
fn a_rising_boot_seq_marks_a_reboot_and_its_subscriptions_are_replaced() {
    let mut st = HouseholdState::default();
    st.apply_topology(&zgs(5, 9));
    let houses = [st];
    let w = wanted(&houses);
    let fake = Fake::default();
    let mut subs = Subscriptions::default();
    let now = Instant::now();
    subs.sync(&fake, &w, url, now);

    assert!(
        subs.reboots(zgs(5, 9).boot_seqs()).is_empty(),
        "first sighting only records"
    );
    assert!(subs.reboots(zgs(5, 9).boot_seqs()).is_empty(), "unchanged");
    let rebooted = subs.reboots(zgs(5, 10).boot_seqs());
    let study = fsonos_types::PlayerId("RINCON_000E58A0000201400".into());
    assert_eq!(rebooted, std::slice::from_ref(&study));

    let mine = w.iter().filter(|x| x.player == study).count();
    let before = fake.next.get();
    let report = subs.resubscribe(&fake, &study, url, now);
    assert_eq!(report.resubscribed, mine);
    assert_eq!(
        fake.next.get() - before,
        u32::try_from(mine).unwrap(),
        "fresh SUBSCRIBEs"
    );
    assert!(
        !fake
            .log
            .borrow()
            .iter()
            .any(|l| l.starts_with("UNSUBSCRIBE")),
        "the rebooted player already forgot its SIDs"
    );
    assert_eq!(subs.len(), w.len());

    // A survey that sees the reboot drops the stale ones for sync to replace.
    assert_eq!(subs.forget(&study), mine);
    assert_eq!(subs.len(), w.len() - mine);
}

#[test]
fn a_subscribe_that_failed_is_retried_on_its_own() {
    let houses = [household(ZGS_S1)];
    let w = wanted(&houses);
    let fake = Fake::default();
    fake.refuse.set(1);
    let mut subs = Subscriptions::default();
    let t0 = Instant::now();
    let report = subs.sync(&fake, &w, url, t0);
    assert_eq!(report.failed.len(), 1);
    assert_eq!(subs.len(), w.len() - 1);
    assert!(
        subs.next_due().unwrap() <= t0 + events::RETRY,
        "the retry is scheduled"
    );

    // Not yet due: nothing happens.
    let early = subs.renew_due(&fake, url, t0 + Duration::from_secs(1));
    assert_eq!((early.subscribed, early.failed.len()), (0, 0));
    // Due: it is subscribed, and its player counts as having answered.
    let retried = subs.renew_due(&fake, url, t0 + events::RETRY);
    assert_eq!(retried.subscribed, 1);
    assert_eq!(retried.answered.len(), 1);
    assert_eq!(subs.len(), w.len());
}
