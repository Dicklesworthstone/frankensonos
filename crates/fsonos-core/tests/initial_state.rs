//! A SUBSCRIBE response is not proof that the callback received initial state.
//! Exercise the production subscription timer with explicit monotonic instants.

use fsonos_core::events::{
    INITIAL_STATE_WAIT, RETRY, Report, Service, Subscriber, Subscriptions, Want,
};
use fsonos_proto::ProtoError;
use fsonos_proto::gena::{Notify, Subscription};
use fsonos_types::PlayerId;
use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

struct Silent {
    timeout: u32,
    subscribes: Cell<u32>,
    renewals: Cell<u32>,
    refuse: Cell<bool>,
    replace_on_renew: Cell<bool>,
    unsubscribed: RefCell<Vec<String>>,
}

impl Silent {
    fn new(timeout: u32) -> Self {
        Self {
            timeout,
            subscribes: Cell::new(0),
            renewals: Cell::new(0),
            refuse: Cell::new(false),
            replace_on_renew: Cell::new(false),
            unsubscribed: RefCell::new(Vec::new()),
        }
    }
}

impl Subscriber for Silent {
    fn subscribe(&self, want: &Want, url: &str) -> Result<Subscription, ProtoError> {
        assert_eq!(url, callback(want.service));
        self.subscribes.set(self.subscribes.get() + 1);
        if self.refuse.get() {
            return Err(ProtoError::Network {
                target: "test-player".into(),
                detail: "injected subscription failure".into(),
            });
        }
        Ok(Subscription {
            sid: format!("uuid:test-{}", self.subscribes.get()),
            timeout_secs: self.timeout,
        })
    }

    fn renew(&self, _: &Want, sid: &str) -> Result<Subscription, ProtoError> {
        self.renewals.set(self.renewals.get() + 1);
        Ok(Subscription {
            sid: if self.replace_on_renew.replace(false) {
                "uuid:renewed".into()
            } else {
                sid.into()
            },
            timeout_secs: self.timeout,
        })
    }

    fn unsubscribe(&self, _: &Want, sid: &str) -> Result<(), ProtoError> {
        self.unsubscribed.borrow_mut().push(sid.into());
        Ok(())
    }
}

fn want(service: Service) -> Want {
    Want {
        player: PlayerId("test-player".into()),
        host: std::net::Ipv4Addr::LOCALHOST.into(),
        service,
    }
}

fn callback(service: Service) -> String {
    format!("http://127.0.0.1:3400/{}", service.tag())
}

fn initial(sid: &str) -> Notify {
    Notify {
        sid: sid.into(),
        seq: 0,
        path: "/".into(),
        properties: Vec::new(),
    }
}

fn accept(subs: &mut Subscriptions, sid: &str) {
    let notify = initial(sid);
    assert!(subs.route(&notify).is_some());
    assert!(!subs.missed(&notify));
}

#[test]
fn a_quiet_subscription_is_replaced_without_waiting_for_a_delta_or_lease() {
    for service in [
        Service::AvTransport,
        Service::RenderingControl,
        Service::GroupRenderingControl,
        Service::ZoneGroupTopology,
    ] {
        let s = Silent::new(1800);
        let mut subs = Subscriptions::default();
        let now = Instant::now();
        subs.sync(&s, &[want(service)], callback, now);
        for attempt in 1..=3 {
            let due = now + INITIAL_STATE_WAIT * attempt;
            assert_eq!(subs.next_due(), Some(due));
            let before = due.checked_sub(Duration::from_millis(1)).unwrap();
            let early = subs.renew_due(&s, callback, before);
            assert_eq!(early, Report::default());
            let recovered = subs.renew_due(&s, callback, due);
            assert_eq!(recovered.resubscribed, 1);
            assert_eq!(recovered.renewed, 0);
            assert!(recovered.failed.is_empty());
            assert_eq!(s.subscribes.get(), attempt + 1);
            let old = format!("uuid:test-{attempt}");
            assert!(subs.route(&initial(&old)).is_none());
            assert_eq!(s.unsubscribed.borrow().last(), Some(&old));
        }
        assert_eq!(s.renewals.get(), 0);
        assert_eq!(subs.len(), 1);
    }
}

#[test]
fn accepted_initial_state_disarms_the_watchdog_for_a_healthy_quiet_stream() {
    let s = Silent::new(300);
    let mut subs = Subscriptions::default();
    let now = Instant::now();
    subs.sync(&s, &[want(Service::AvTransport)], callback, now);
    accept(&mut subs, "uuid:test-1");
    let early = subs.renew_due(&s, callback, now + INITIAL_STATE_WAIT);
    assert_eq!(early, Report::default());
    for cycle in 1..=3 {
        let due = now + Duration::from_secs(150 * cycle);
        assert_eq!(subs.next_due(), Some(due));
        assert_eq!(subs.renew_due(&s, callback, due).renewed, 1);
    }
    assert_eq!(s.subscribes.get(), 1);
    assert!(s.unsubscribed.borrow().is_empty());
}

#[test]
fn successful_short_lease_renewals_do_not_slide_the_initial_deadline() {
    let s = Silent::new(10);
    let mut subs = Subscriptions::default();
    let now = Instant::now();
    subs.sync(&s, &[want(Service::AvTransport)], callback, now);
    for seconds in [5, 10, 15, 20, 25] {
        let due = now + Duration::from_secs(seconds);
        assert_eq!(subs.next_due(), Some(due));
        assert_eq!(subs.renew_due(&s, callback, due).renewed, 1);
    }
    assert_eq!(subs.next_due(), Some(now + INITIAL_STATE_WAIT));
    let recovered = subs.renew_due(&s, callback, now + INITIAL_STATE_WAIT);
    assert_eq!(recovered.resubscribed, 1);
    assert_eq!(recovered.renewed, 0);
    assert_eq!(s.renewals.get(), 5);
    assert_eq!(s.subscribes.get(), 2);
}

#[test]
fn a_new_sid_from_renewal_gets_its_own_initial_state_deadline() {
    let s = Silent::new(300);
    let mut subs = Subscriptions::default();
    let now = Instant::now();
    subs.sync(&s, &[want(Service::AvTransport)], callback, now);
    accept(&mut subs, "uuid:test-1");
    s.replace_on_renew.set(true);
    let renewed = now + Duration::from_secs(150);
    assert_eq!(subs.renew_due(&s, callback, renewed).renewed, 1);
    assert_eq!(subs.next_due(), Some(renewed + INITIAL_STATE_WAIT));
    assert!(subs.route(&initial("uuid:test-1")).is_none());
    let recovered = subs.renew_due(&s, callback, renewed + INITIAL_STATE_WAIT);
    assert_eq!(recovered.resubscribed, 1);
    assert_eq!(s.unsubscribed.borrow().as_slice(), ["uuid:renewed"]);
    accept(&mut subs, "uuid:test-2");
}

#[test]
fn a_failed_watchdog_replacement_keeps_the_old_sid_and_respects_backoff() {
    let s = Silent::new(300);
    let mut subs = Subscriptions::default();
    let now = Instant::now();
    let wanted = [want(Service::AvTransport)];
    subs.sync(&s, &wanted, callback, now);
    s.refuse.set(true);
    let expired = now + INITIAL_STATE_WAIT;
    let failed = subs.renew_due(&s, callback, expired);
    assert_eq!(failed.failed.len(), 1);
    assert_eq!(subs.next_due(), Some(expired + RETRY));
    assert!(subs.route(&initial("uuid:test-1")).is_some());
    for seconds in 1..30 {
        let at = expired + Duration::from_secs(seconds);
        assert_eq!(subs.sync(&s, &wanted, callback, at), Report::default());
        assert_eq!(subs.renew_due(&s, callback, at), Report::default());
    }
    assert_eq!(s.subscribes.get(), 2);
    assert_eq!(s.renewals.get(), 0);
    assert!(s.unsubscribed.borrow().is_empty());
    s.refuse.set(false);
    let recovered = subs.renew_due(&s, callback, expired + RETRY);
    assert_eq!(recovered.resubscribed, 1);
    assert_eq!(s.subscribes.get(), 3);
    assert!(subs.route(&initial("uuid:test-1")).is_none());
    accept(&mut subs, "uuid:test-3");
    assert_eq!(
        subs.next_due(),
        Some(expired + RETRY + Duration::from_secs(150))
    );
}

#[test]
fn each_service_needs_its_own_initial_state_even_on_the_same_player() {
    let s = Silent::new(300);
    let mut subs = Subscriptions::default();
    let now = Instant::now();
    let wanted = [
        want(Service::AvTransport),
        want(Service::RenderingControl),
        want(Service::GroupRenderingControl),
        want(Service::ZoneGroupTopology),
    ];
    subs.sync(&s, &wanted, callback, now);
    for sid in ["uuid:test-2", "uuid:test-3", "uuid:test-4"] {
        accept(&mut subs, sid);
    }
    let recovered = subs.renew_due(&s, callback, now + INITIAL_STATE_WAIT);
    assert_eq!(recovered.resubscribed, 1);
    assert_eq!(s.unsubscribed.borrow().as_slice(), ["uuid:test-1"]);
    assert_eq!(subs.len(), 4);
    accept(&mut subs, "uuid:test-5");
    assert_eq!(subs.next_due(), Some(now + Duration::from_secs(150)));
}

#[test]
fn a_subscription_that_initially_failed_gets_a_full_window_after_it_succeeds() {
    let s = Silent::new(300);
    let mut subs = Subscriptions::default();
    let now = Instant::now();
    s.refuse.set(true);
    subs.sync(&s, &[want(Service::AvTransport)], callback, now);
    s.refuse.set(false);
    assert_eq!(subs.renew_due(&s, callback, now + RETRY).subscribed, 1);
    assert_eq!(subs.next_due(), Some(now + RETRY + INITIAL_STATE_WAIT));
    let due = now + RETRY + INITIAL_STATE_WAIT;
    let early = due.checked_sub(Duration::from_millis(1)).unwrap();
    assert_eq!(subs.renew_due(&s, callback, early), Report::default());
    let recovered = subs.renew_due(&s, callback, due);
    assert_eq!(recovered.resubscribed, 1);
}

#[test]
fn routing_without_accepting_state_does_not_disarm_the_watchdog() {
    let s = Silent::new(300);
    let mut subs = Subscriptions::default();
    let now = Instant::now();
    subs.sync(&s, &[want(Service::AvTransport)], callback, now);
    assert!(subs.route(&initial("uuid:test-1")).is_some());
    assert!(subs.route(&initial("uuid:stray")).is_none());
    assert!(!subs.missed(&initial("uuid:stray")));
    assert_eq!(subs.next_due(), Some(now + INITIAL_STATE_WAIT));
    let recovered = subs.renew_due(&s, callback, now + INITIAL_STATE_WAIT);
    assert_eq!(recovered.resubscribed, 1);
}

#[test]
fn forgetting_dropping_or_shutting_down_cancels_initial_state_recovery() {
    for mode in 0..3 {
        let s = Silent::new(300);
        let mut subs = Subscriptions::default();
        let now = Instant::now();
        let wanted = want(Service::AvTransport);
        subs.sync(&s, std::slice::from_ref(&wanted), callback, now);
        match mode {
            0 => assert_eq!(subs.forget(&wanted.player), 1),
            1 => assert_eq!(subs.sync(&s, &[], callback, now).dropped, 1),
            _ => subs.unsubscribe_all(&s),
        }
        assert_eq!(subs.next_due(), None);
        let after = subs.renew_due(&s, callback, now + INITIAL_STATE_WAIT * 2);
        assert_eq!(after, Report::default());
        assert_eq!(s.subscribes.get(), 1);
    }
}
