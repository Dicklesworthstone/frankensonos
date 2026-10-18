use super::*;
use crate::events::maintenance::Provider;
use crate::events::{RETRY, Service, Subscriber, Want};
use crate::playback::{EventSource, Playback};
use fsonos_proto::ProtoError;
use fsonos_proto::control::set_volume;
use fsonos_proto::gena::Subscription;
use fsonos_proto::net::{EventSink, Lan};
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use fsonos_types::PlayerId;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(10);

#[derive(Default)]
struct Gate {
    entered: AtomicBool,
    released: AtomicBool,
}

struct Release(Arc<Gate>);

impl Drop for Release {
    fn drop(&mut self) {
        self.0.released.store(true, Ordering::Release);
    }
}

struct Held {
    inner: Arc<Lan>,
    gate: Arc<Gate>,
    subscribed: AtomicUsize,
    renewed: AtomicUsize,
    retired: Mutex<Vec<String>>,
}

impl Subscriber for Held {
    fn subscribe(&self, want: &Want, callback: &str) -> Result<Subscription, ProtoError> {
        self.subscribed.fetch_add(1, Ordering::Relaxed);
        let result = Subscriber::subscribe(&*self.inner, want, callback)?;
        self.gate.entered.store(true, Ordering::Release);
        wait_for(|| self.gate.released.load(Ordering::Acquire));
        Ok(result)
    }

    fn renew(&self, want: &Want, sid: &str) -> Result<Subscription, ProtoError> {
        self.renewed.fetch_add(1, Ordering::Relaxed);
        Subscriber::renew(&*self.inner, want, sid)
    }

    fn unsubscribe(&self, want: &Want, sid: &str) -> Result<(), ProtoError> {
        let result = Subscriber::unsubscribe(&*self.inner, want, sid);
        self.retired.lock().unwrap().push(sid.into());
        result
    }
}

struct Fixture {
    // Unblock HTTP reply holders before network fixtures drop on a panic.
    _release: Release,
    sim: SimHandle,
    sink: EventSink,
    provider: Arc<Held>,
    subs: Subscriptions,
    stop: Arc<AtomicBool>,
    want: Want,
}

impl Fixture {
    fn new(generation: u8) -> Self {
        let builder = SimHousehold::builder();
        let sim = if generation == 1 {
            builder.s1([SimPlayerSpec::new("Room", SimModel::Play5Gen1)])
        } else {
            builder.s2([SimPlayerSpec::new("Room", SimModel::One)])
        }
        .spawn()
        .unwrap();
        let lan = Arc::new(
            Lan::start()
                .unwrap()
                .with_routes(sim.players().iter().map(|p| (p.ip, p.addr)).collect()),
        );
        let player = &sim.players()[0];
        let want = Want {
            player: PlayerId(player.uuid.clone()),
            host: player.ip,
            service: Service::RenderingControl,
        };
        let local = lan.local_address_toward(want.host).unwrap();
        assert!(local.is_loopback());
        let sink = EventSink::start(SocketAddr::new(local, 0)).unwrap();
        let gate = Arc::new(Gate::default());
        let provider = Arc::new(Held {
            inner: lan,
            gate: Arc::clone(&gate),
            subscribed: AtomicUsize::new(0),
            renewed: AtomicUsize::new(0),
            retired: Mutex::new(Vec::new()),
        });
        let mut subs = Subscriptions::default();
        subs.defer_network();
        let report = subs.sync(
            &*provider,
            std::slice::from_ref(&want),
            |s| sink.callback_url(s.tag()),
            Instant::now(),
        );
        assert_eq!(report.subscribed, 0);
        Self {
            _release: Release(gate),
            sim,
            sink,
            provider,
            subs,
            stop: Arc::new(AtomicBool::new(false)),
            want,
        }
    }

    fn start(&mut self) {
        let provider: Provider = self.provider.clone();
        let _ = self.subs.poll_renewals(self.stop.load(Ordering::Acquire));
        self.subs.start_renewals(
            &provider,
            |s| self.sink.callback_url(s.tag()),
            &self.stop,
            Instant::now(),
        );
    }

    fn begin(&mut self) -> Notify {
        self.start();
        wait_for(|| self.provider.gate.entered.load(Ordering::Acquire));
        let initial = self.sink.recv_or_closed(WAIT).unwrap().unwrap();
        assert_eq!(initial.seq, 0);
        assert!(self.subs.route(&initial).is_none());
        initial
    }

    fn volume(&self, value: u8, sid: &str) -> Notify {
        set_volume(&*self.provider.inner, self.want.host, value).unwrap();
        self.receive(sid)
    }

    fn receive(&self, sid: &str) -> Notify {
        let deadline = Instant::now() + WAIT;
        loop {
            assert!(Instant::now() < deadline, "wanted callback did not arrive");
            let notify = self.sink.recv_or_closed(WAIT).unwrap().unwrap();
            if notify.sid == sid {
                return notify;
            }
        }
    }

    fn adopt(&mut self) {
        self.provider.gate.released.store(true, Ordering::Release);
        wait_for(|| {
            let _ = self.subs.poll_renewals(false);
            self.subs.maintenance.jobs.is_empty()
        });
    }

    fn apply(&mut self, playback: &mut Playback, notify: &Notify) {
        assert!(self.subs.route(notify).is_some());
        playback
            .apply(
                &self.want.player,
                EventSource::RenderingControl,
                notify,
                Instant::now(),
            )
            .unwrap();
        let _ = self.subs.missed(notify);
    }

    fn replay(&mut self, playback: &mut Playback) {
        while let Some(notify) = self.subs.take_early() {
            self.apply(playback, &notify);
        }
        assert_eq!(self.subs.maintenance.early_bytes, 0);
    }

    fn repair(&mut self, playback: &mut Playback, expected: u8) {
        assert!(self.subs.active[0].refresh_pending);
        let deadline = self.subs.active[0].renew_at;
        assert!(deadline > Instant::now() + RETRY / 2);
        let calls = self.provider.subscribed.load(Ordering::Relaxed);
        let sid = self.subs.active[0].sid.clone();
        for _ in 0..100 {
            assert!(self.subs.defer_refresh(&sid, Instant::now()));
            self.start();
            assert_eq!(self.subs.active[0].renew_at, deadline);
        }
        assert_eq!(self.provider.subscribed.load(Ordering::Relaxed), calls);
        // Advance only the local deadline, without waiting through the cooldown.
        self.subs.active[0].renew_at = Instant::now();
        let soap_calls = self.sim.soap_log().len();
        self.start();
        self.adopt();
        let repaired_sid = self.subs.active[0].sid.clone();
        assert_ne!(repaired_sid, sid);
        let full = self.receive(&repaired_sid);
        assert_eq!(full.seq, 0);
        self.apply(playback, &full);
        assert_eq!(
            playback.of(&self.want.player).unwrap().volume,
            Some(expected)
        );
        assert!(!self.subs.active[0].refresh_pending);
        assert_eq!(self.provider.subscribed.load(Ordering::Relaxed), calls + 1);
        assert_eq!(self.provider.renewed.load(Ordering::Relaxed), 0);
        assert_eq!(
            self.sim.soap_log().len(),
            soap_calls,
            "repair never writes playback"
        );
    }

    fn finish(mut self) {
        self.provider.gate.released.store(true, Ordering::Release);
        self.stop.store(true, Ordering::Release);
        wait_for(|| {
            let _ = self.subs.poll_renewals(true);
            self.subs.maintenance.jobs.is_empty()
        });
        self.subs.unsubscribe_all(&*self.provider.inner);
        self.sim.shutdown();
    }
}

fn wait_for(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "early callback test did not converge"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn a_count_overflow_recovers_the_lost_tail_without_waiting_for_another_event() {
    for generation in [1, 2] {
        let mut f = Fixture::new(generation);
        let initial = f.begin();
        assert!(f.subs.hold_early(&initial));
        for seq in 1..=EARLY_COUNT {
            let notify = f.volume(u8::try_from(seq % 100).unwrap(), &initial.sid);
            assert_eq!(notify.seq, u32::try_from(seq).unwrap());
            assert_eq!(f.subs.hold_early(&notify), seq < EARLY_COUNT);
        }
        assert_eq!(f.subs.maintenance.early.len(), EARLY_COUNT);
        assert!(f.subs.maintenance.early_bytes <= EARLY_BYTES);
        assert!(f.subs.maintenance.jobs[0].early_loss);
        f.adopt();
        let mut playback = Playback::default();
        f.replay(&mut playback);
        assert_eq!(playback.of(&f.want.player).unwrap().volume, Some(27));
        assert_eq!(f.subs.active[0].last_seq, Some(127));
        f.repair(&mut playback, 28);
        f.finish();
    }
}

#[test]
fn an_oversized_tail_is_not_retained_but_still_requests_full_state() {
    let mut f = Fixture::new(2);
    let initial = f.begin();
    assert!(f.subs.hold_early(&initial));
    let before = f.subs.maintenance.early_bytes;
    let mut tail = f.volume(59, &initial.sid);
    tail.properties
        .push(("Padding".into(), "x".repeat(EARLY_BYTES)));
    assert!(!f.subs.hold_early(&tail));
    assert_eq!(f.subs.maintenance.early_bytes, before);
    assert_eq!(f.subs.maintenance.early.len(), 1);
    f.adopt();
    let mut playback = Playback::default();
    f.replay(&mut playback);
    f.repair(&mut playback, 59);
    f.finish();
}

#[test]
fn expiry_before_adoption_leaves_loss_evidence_and_frees_space_immediately() {
    let mut f = Fixture::new(2);
    let initial = f.begin();
    assert!(f.subs.hold_early(&initial));
    f.subs.maintenance.early[0].at = Instant::now().checked_sub(EARLY_AGE).unwrap();
    let latest = f.volume(61, &initial.sid);
    // Admission itself expires the initial; no intervening take_early is needed.
    assert!(f.subs.hold_early(&latest));
    assert_eq!(f.subs.maintenance.early.len(), 1);
    assert!(f.subs.maintenance.jobs[0].early_loss);
    f.adopt();
    let mut playback = Playback::default();
    f.replay(&mut playback);
    f.repair(&mut playback, 61);
    f.finish();
}

#[test]
fn expiry_after_the_initial_was_applied_still_recovers_a_quiet_lost_tail() {
    let mut f = Fixture::new(2);
    let initial = f.begin();
    assert!(f.subs.hold_early(&initial));
    let tail = f.volume(63, &initial.sid);
    assert!(f.subs.hold_early(&tail));
    f.adopt();
    let mut playback = Playback::default();
    let first = f.subs.take_early().unwrap();
    f.apply(&mut playback, &first);
    assert!(!f.subs.active[0].refresh_pending);
    f.subs.maintenance.early[0].at = Instant::now().checked_sub(EARLY_AGE).unwrap();
    assert!(f.subs.take_early().is_none());
    assert_eq!(f.subs.maintenance.early_bytes, 0);
    f.repair(&mut playback, 63);
    f.finish();
}

#[test]
fn expired_duplicates_and_unrelated_service_traffic_do_not_request_replacement() {
    let mut f = Fixture::new(2);
    let initial = f.begin();
    let mut unrelated = initial.clone();
    unrelated.path = "/AVTransport".into();
    unrelated
        .properties
        .push(("Padding".into(), "x".repeat(EARLY_BYTES)));
    for _ in 0..100 {
        assert!(!f.subs.hold_early(&unrelated));
    }
    assert!(!f.subs.maintenance.jobs[0].early_loss);
    assert!(f.subs.hold_early(&initial));
    let tail = f.volume(67, &initial.sid);
    assert!(f.subs.hold_early(&tail));
    f.adopt();
    let mut playback = Playback::default();
    let first = f.subs.take_early().unwrap();
    f.apply(&mut playback, &first);
    // A duplicate arrived through normal intake before its stored copy expired.
    f.apply(&mut playback, &tail);
    f.subs.maintenance.early[0].at = Instant::now().checked_sub(EARLY_AGE).unwrap();
    assert!(f.subs.take_early().is_none());
    assert_eq!(f.subs.maintenance.early_bytes, 0);
    assert!(!f.subs.active[0].refresh_pending);
    assert_eq!(f.provider.subscribed.load(Ordering::Relaxed), 1);
    f.finish();
}

#[test]
fn losing_an_early_event_cannot_resurrect_a_cancelled_subscription_request() {
    let mut f = Fixture::new(2);
    let mut initial = f.begin();
    initial
        .properties
        .push(("Padding".into(), "x".repeat(EARLY_BYTES)));
    assert!(!f.subs.hold_early(&initial));
    assert!(f.subs.maintenance.jobs[0].early_loss);
    f.subs.forget(&f.want.player);
    f.adopt();
    assert!(f.subs.active.is_empty());
    assert!(f.subs.pending.is_empty());
    assert!(f.subs.take_early().is_none());
    assert_eq!(f.provider.subscribed.load(Ordering::Relaxed), 1);
    assert_eq!(f.provider.retired.lock().unwrap().as_slice(), [initial.sid]);
    f.finish();
}

#[test]
fn unknown_overflow_does_not_turn_a_same_sid_renewal_into_a_replacement() {
    let mut f = Fixture::new(2);
    let initial = f.begin();
    assert!(f.subs.hold_early(&initial));
    f.adopt();
    let mut playback = Playback::default();
    f.replay(&mut playback);
    f.subs.active[0].renew_at = Instant::now();
    f.start();
    let mut unknown = initial.clone();
    unknown.sid = "uuid:unrelated-early-overflow".into();
    unknown
        .properties
        .push(("Padding".into(), "x".repeat(EARLY_BYTES)));
    assert!(!f.subs.hold_early(&unknown));
    assert!(f.subs.maintenance.jobs[0].early_loss);
    f.adopt();
    assert_eq!(f.subs.active[0].sid, initial.sid);
    assert_eq!(f.subs.active[0].last_seq, Some(0));
    assert!(!f.subs.active[0].refresh_pending);
    assert_eq!(f.provider.subscribed.load(Ordering::Relaxed), 1);
    assert_eq!(f.provider.renewed.load(Ordering::Relaxed), 1);
    f.finish();
}

#[test]
fn global_callback_loss_marks_in_flight_initials_and_recovers_without_a_later_gap() {
    for generation in [1, 2] {
        let mut f = Fixture::new(generation);
        let initial = f.begin();
        assert!(f.subs.hold_early(&initial));
        let tail = f.volume(73, &initial.sid);
        assert_eq!(tail.seq, 1);
        // The raw inbox lost this tail before it could reach hold_early.
        f.subs.callbacks_lost(Instant::now());
        assert!(f.subs.maintenance.jobs[0].early_loss);
        f.adopt();
        let mut playback = Playback::default();
        f.replay(&mut playback);
        f.repair(&mut playback, 73);
        f.finish();
    }
}

#[test]
fn global_loss_preserves_live_cursors_and_existing_failed_replacement_backoff() {
    let mut f = Fixture::new(2);
    let initial = f.begin();
    assert!(f.subs.hold_early(&initial));
    f.adopt();
    let mut playback = Playback::default();
    f.replay(&mut playback);
    let now = Instant::now();
    f.subs.callbacks_lost(now);
    let deadline = f.subs.active[0].renew_at;
    assert_eq!(deadline, now + RETRY);
    assert_eq!(f.subs.active[0].sid, initial.sid);
    assert_eq!(f.subs.active[0].last_seq, Some(0));
    for _ in 0..100 {
        f.subs.callbacks_lost(Instant::now());
        f.start();
        assert_eq!(f.subs.active[0].renew_at, deadline);
    }
    // A failed request may impose a later retry than this local loss cooldown.
    f.subs.active[0].renew_at = now + RETRY * 2;
    f.subs.callbacks_lost(Instant::now());
    assert_eq!(f.subs.active[0].renew_at, now + RETRY * 2);
    assert_eq!(f.provider.subscribed.load(Ordering::Relaxed), 1);
    assert_eq!(f.provider.renewed.load(Ordering::Relaxed), 0);
    f.finish();
}
