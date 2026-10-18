use super::*;
use crate::events::RETRY;
use crate::playback::{EventSource, Playback};
use fsonos_proto::control::set_volume;
use fsonos_proto::net::{EventSink, Lan};
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use std::net::SocketAddr;
use std::sync::atomic::AtomicUsize;

const WAIT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Renew,
    Subscribe,
    Cleanup,
}

#[derive(Default)]
struct Gate {
    entered: AtomicUsize,
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
    stage: Stage,
    armed: AtomicBool,
    fail_renew: AtomicBool,
    fail_subscribe: AtomicBool,
    renewals: AtomicUsize,
    subscriptions: AtomicUsize,
    retired: Mutex<Vec<String>>,
}

impl Held {
    fn hold(&self, stage: Stage) {
        if self.stage != stage || !self.armed.load(Ordering::Acquire) {
            return;
        }
        self.gate.entered.fetch_add(1, Ordering::Release);
        let deadline = Instant::now() + WAIT;
        while !self.gate.released.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "held lease request timed out");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

impl Subscriber for Held {
    fn subscribe(&self, want: &Want, callback: &str) -> Result<Subscription, ProtoError> {
        self.subscriptions.fetch_add(1, Ordering::Relaxed);
        if self.fail_subscribe.load(Ordering::Acquire) {
            return Err(ProtoError::NotWired("injected subscribe failure"));
        }
        let result = Subscriber::subscribe(&*self.inner, want, callback)?;
        self.hold(Stage::Subscribe);
        Ok(result)
    }

    fn renew(&self, want: &Want, sid: &str) -> Result<Subscription, ProtoError> {
        self.renewals.fetch_add(1, Ordering::Relaxed);
        self.hold(Stage::Renew);
        if self.fail_renew.load(Ordering::Acquire) {
            return Err(ProtoError::NotWired("injected renewal failure"));
        }
        Subscriber::renew(&*self.inner, want, sid)
    }

    fn unsubscribe(&self, want: &Want, sid: &str) -> Result<(), ProtoError> {
        self.hold(Stage::Cleanup);
        let result = Subscriber::unsubscribe(&*self.inner, want, sid);
        self.retired.lock().unwrap().push(sid.into());
        result
    }
}

struct Fixture {
    _release: Release,
    sim: SimHandle,
    sink: EventSink,
    provider: Arc<Held>,
    subs: Subscriptions,
    stop: Arc<AtomicBool>,
    wants: Vec<Want>,
}

impl Fixture {
    fn new(generation: u8, stage: Stage, count: usize) -> Self {
        let hardware = if generation == 1 {
            SimModel::Play5Gen1
        } else {
            SimModel::One
        };
        let rooms = ["A", "B", "C", "D", "E"]
            .into_iter()
            .take(count)
            .map(|name| SimPlayerSpec::new(name, hardware))
            .collect::<Vec<_>>();
        let builder = SimHousehold::builder();
        let sim = if generation == 1 {
            builder.s1(rooms)
        } else {
            builder.s2(rooms)
        }
        .spawn()
        .unwrap();
        let lan = Arc::new(
            Lan::start()
                .unwrap()
                .with_routes(sim.players().iter().map(|p| (p.ip, p.addr)).collect()),
        );
        let local = lan.local_address_toward(sim.players()[0].ip).unwrap();
        assert!(local.is_loopback());
        let sink = EventSink::start(SocketAddr::new(local, 0)).unwrap();
        let wants: Vec<_> = sim
            .players()
            .iter()
            .map(|p| Want {
                player: PlayerId(p.uuid.clone()),
                host: p.ip,
                service: Service::RenderingControl,
            })
            .collect();
        let gate = Arc::new(Gate::default());
        let provider = Arc::new(Held {
            inner: lan,
            gate: Arc::clone(&gate),
            stage,
            armed: AtomicBool::new(false),
            fail_renew: AtomicBool::new(false),
            fail_subscribe: AtomicBool::new(false),
            renewals: AtomicUsize::new(0),
            subscriptions: AtomicUsize::new(0),
            retired: Mutex::new(Vec::new()),
        });
        let mut subs = Subscriptions::default();
        let report = subs.sync(
            &*provider,
            &wants,
            |s| sink.callback_url(s.tag()),
            Instant::now(),
        );
        assert_eq!(report.subscribed, count);
        for _ in 0..count {
            let notify = sink.recv_or_closed(WAIT).unwrap().unwrap();
            assert!(subs.route(&notify).is_some());
            assert!(!subs.missed(&notify));
        }
        // The remote leases are live; only the client's renewal schedule is old.
        for a in &mut subs.active {
            a.renew_at = Instant::now();
        }
        provider.armed.store(true, Ordering::Release);
        Self {
            _release: Release(gate),
            sim,
            sink,
            provider,
            subs,
            stop: Arc::new(AtomicBool::new(false)),
            wants,
        }
    }

    fn start(&mut self) -> Vec<(Report, Instant)> {
        let sink = &self.sink;
        let provider: Provider = self.provider.clone();
        let reports = self.subs.poll_renewals(self.stop.load(Ordering::Acquire));
        self.subs.start_renewals(
            &provider,
            |s| sink.callback_url(s.tag()),
            &self.stop,
            Instant::now(),
        );
        reports
    }

    /// Accept a routed rendering event into playback and its sequence cursor.
    fn accept_volume(&mut self, playback: &mut Playback, event: &Notify, volume: u8) {
        let player = &self.wants[0].player;
        assert!(self.subs.route(event).is_some());
        playback
            .apply(player, EventSource::RenderingControl, event, Instant::now())
            .unwrap();
        assert!(!self.subs.missed(event));
        assert_eq!(playback.of(player).unwrap().volume, Some(volume));
    }

    fn release(&self) {
        self.provider.gate.released.store(true, Ordering::Release);
    }

    fn drain(&mut self) -> Vec<(Report, Instant)> {
        let mut reports = Vec::new();
        wait_for(|| {
            reports.extend(self.subs.poll_renewals(false));
            self.subs.maintenance.jobs.is_empty()
        });
        reports
    }

    fn finish(mut self) {
        self.release();
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
            "lease maintenance did not converge"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn notify(sid: &str, seq: u32) -> Notify {
    Notify {
        sid: sid.into(),
        seq,
        path: "/RenderingControl".into(),
        properties: Vec::new(),
    }
}

#[test]
fn renewal_does_not_block_event_cursors_and_preserves_initial_evidence_on_s1_and_s2() {
    for generation in [1, 2] {
        let mut f = Fixture::new(generation, Stage::Renew, 1);
        let before = f.subs.active[0].clone();
        let started = Instant::now();
        assert!(f.start().is_empty());
        assert!(started.elapsed() < Duration::from_secs(1));
        wait_for(|| f.provider.gate.entered.load(Ordering::Acquire) == 1);
        set_volume(&*f.provider.inner, f.wants[0].host, 29).unwrap();
        let n = f.sink.recv_or_closed(WAIT).unwrap().unwrap();
        assert_eq!(n.sid, before.sid);
        assert!(f.subs.route(&n).is_some());
        assert!(!f.subs.missed(&n));
        f.release();
        let reports = f.drain();
        assert_eq!(reports.iter().map(|(r, _)| r.renewed).sum::<usize>(), 1);
        let after = &f.subs.active[0];
        assert_eq!(after.sid, before.sid);
        assert_eq!(after.last_seq, Some(n.seq));
        assert_eq!(after.initial_deadline, before.initial_deadline);
        assert!(after.renew_at > Instant::now());
        assert!(f.subs.route(&n).is_none());
        assert!(f.provider.retired.lock().unwrap().is_empty());
        f.finish();
    }
}

#[test]
fn replacement_initials_without_old_stream_progress_are_replayed_once() {
    for generation in [1, 2] {
        let mut f = Fixture::new(generation, Stage::Subscribe, 1);
        f.provider.fail_renew.store(true, Ordering::Release);
        let old = f.subs.active[0].sid.clone();
        let _ = f.start();
        wait_for(|| f.provider.gate.entered.load(Ordering::Acquire) == 1);
        let initial = f.sink.recv_or_closed(WAIT).unwrap().unwrap();
        assert_ne!(initial.sid, old);
        assert_eq!(initial.seq, 0);
        assert!(f.subs.route(&initial).is_none());
        assert!(f.subs.hold_early(&initial));
        set_volume(&*f.provider.inner, f.wants[0].host, 43).unwrap();
        // Both old and new streams can deliver the change until adoption.
        loop {
            let n = f.sink.recv_or_closed(WAIT).unwrap().unwrap();
            if n.sid == initial.sid {
                assert_eq!(n.seq, 1);
                assert!(f.subs.hold_early(&n));
                break;
            }
            // Deliberately drop the old stream's counterpart here. The
            // separate regression covers accepting it before the cutover.
            assert_eq!(n.sid, old);
        }
        f.release();
        let reports = f.drain();
        assert_eq!(
            reports.iter().map(|(r, _)| r.resubscribed).sum::<usize>(),
            1
        );
        for seq in [0, 1] {
            let n = f.subs.take_early().unwrap();
            assert_eq!(n.sid, initial.sid);
            assert_eq!(n.seq, seq);
            assert!(f.subs.route(&n).is_some());
            assert!(!f.subs.missed(&n));
        }
        assert!(f.subs.take_early().is_none());
        assert_eq!(f.subs.maintenance.early_bytes, 0);
        assert_eq!(f.provider.retired.lock().unwrap().as_slice(), [old]);
        assert_eq!(f.provider.subscriptions.load(Ordering::Relaxed), 2);
        f.finish();
    }
}

#[test]
fn failed_replacement_keeps_the_live_sid_and_counts_backoff_from_completion() {
    let mut f = Fixture::new(2, Stage::Renew, 1);
    let sid = f.subs.active[0].sid.clone();
    f.provider.fail_renew.store(true, Ordering::Release);
    f.provider.fail_subscribe.store(true, Ordering::Release);
    let _ = f.start();
    wait_for(|| f.provider.gate.entered.load(Ordering::Acquire) == 1);
    let forwarded = notify(&sid, 1);
    assert!(!f.subs.missed(&forwarded));
    let released = Instant::now();
    f.release();
    let reports = f.drain();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].0.failed.len(), 1);
    assert_eq!(f.subs.active[0].sid, sid);
    assert!(f.subs.active[0].refresh_pending);
    assert!(f.subs.active[0].renew_at >= released + RETRY);
    assert!(
        f.subs.route(&forwarded).is_none(),
        "failure cannot rewind SEQ"
    );
    assert!(f.subs.route(&notify(&sid, 2)).is_some());
    for _ in 0..100 {
        assert!(f.start().is_empty());
    }
    assert_eq!(f.provider.renewals.load(Ordering::Relaxed), 1);
    assert_eq!(f.provider.subscriptions.load(Ordering::Relaxed), 2);
    f.provider.fail_subscribe.store(false, Ordering::Release);
    f.subs.active[0].renew_at = Instant::now();
    let _ = f.start();
    assert_eq!(
        f.drain().iter().map(|(r, _)| r.resubscribed).sum::<usize>(),
        1
    );
    assert_eq!(
        f.provider.renewals.load(Ordering::Relaxed),
        1,
        "retry SUBSCRIBE, not a stale renewal"
    );
    f.finish();
}

#[test]
fn cancelled_lease_workers_retain_the_shared_bound_and_never_resurrect_forgotten_players() {
    let mut f = Fixture::new(2, Stage::Renew, 5);
    let _ = f.start();
    wait_for(|| f.provider.gate.entered.load(Ordering::Acquire) == MAX_WORKERS);
    let forgotten = f.subs.maintenance.jobs[0].plan.want.player.clone();
    f.subs.forget(&forgotten);
    f.provider.fail_renew.store(true, Ordering::Release);
    for _ in 0..100 {
        let _ = f.start();
    }
    assert_eq!(f.subs.maintenance.jobs.len(), MAX_WORKERS);
    assert_eq!(f.provider.renewals.load(Ordering::Relaxed), MAX_WORKERS);
    f.release();
    let _ = f.drain();
    assert!(f.subs.active.iter().all(|a| a.want.player != forgotten));
    // Three uncancelled workers replace failed renewals; the forgotten one
    // observes cancellation before issuing a follow-up SUBSCRIBE.
    assert_eq!(f.provider.subscriptions.load(Ordering::Relaxed), 8);
    let _ = f.start();
    let _ = f.drain();
    assert_eq!(f.subs.active.len(), 4);
    assert_eq!(f.provider.renewals.load(Ordering::Relaxed), 5);
    f.finish();
}

#[test]
fn a_new_identical_lease_cannot_accept_an_older_result() {
    let mut f = Fixture::new(2, Stage::Renew, 1);
    let _ = f.start();
    wait_for(|| f.provider.gate.entered.load(Ordering::Acquire) == 1);
    let before = f.subs.active[0].clone();
    let mut recreated = before.clone();
    recreated.epoch = Arc::new(());
    f.subs.active[0] = recreated;
    f.subs.invalidate_renewals();
    f.release();
    assert!(f.drain().is_empty());
    assert_eq!(f.subs.active[0].renew_at, before.renew_at);
    assert!(!Arc::ptr_eq(&f.subs.active[0].epoch, &before.epoch));
    f.finish();
}

#[test]
fn slow_retirement_keeps_its_slot_after_new_state_is_adopted() {
    let mut f = Fixture::new(2, Stage::Cleanup, 1);
    f.provider.fail_renew.store(true, Ordering::Release);
    let sid = f.subs.active[0].sid.clone();
    let _ = f.start();
    wait_for(|| {
        let _ = f.subs.poll_renewals(false);
        f.subs.active[0].sid != sid
    });
    wait_for(|| f.provider.gate.entered.load(Ordering::Acquire) == 1);
    assert_eq!(f.subs.maintenance.jobs.len(), 1);
    assert!(f.subs.maintenance.jobs[0].finalized);
    f.subs.active[0].renew_at = Instant::now();
    for _ in 0..100 {
        let _ = f.start();
    }
    assert_eq!(f.provider.renewals.load(Ordering::Relaxed), 1);
    assert_eq!(f.subs.maintenance.jobs.len(), 1);
    f.release();
    let _ = f.drain();
    assert_eq!(f.provider.retired.lock().unwrap().as_slice(), [sid]);
    f.finish();
}

#[test]
fn early_event_storage_is_bounded_by_count_bytes_and_lifetime() {
    let mut f = Fixture::new(2, Stage::Renew, 1);
    let _ = f.start();
    wait_for(|| f.provider.gate.entered.load(Ordering::Acquire) == 1);
    assert!(!f.subs.hold_early(&notify(&f.subs.active[0].sid.clone(), 0)));
    for seq in 0..u32::try_from(EARLY_COUNT).unwrap() {
        assert!(f.subs.hold_early(&notify("uuid:unknown-test", seq)));
    }
    assert!(!f.subs.hold_early(&notify("uuid:over-count", 0)));
    for early in &mut f.subs.maintenance.early {
        early.at = Instant::now().checked_sub(EARLY_AGE).unwrap();
    }
    assert!(f.subs.take_early().is_none());
    assert_eq!(f.subs.maintenance.early_bytes, 0);
    let mut large = notify("uuid:large", 0);
    large
        .properties
        .push(("LastChange".into(), "x".repeat(EARLY_BYTES)));
    assert!(!f.subs.hold_early(&large));
    assert!(f.subs.hold_early(&notify("uuid:unmatched", 0)));
    f.release();
    let _ = f.drain();
    assert!(f.subs.take_early().is_none());
    assert_eq!(f.subs.maintenance.early_bytes, 0);
    f.finish();
}

#[test]
fn stopping_during_renewal_prevents_replacement_and_does_not_publish_failure() {
    let mut f = Fixture::new(2, Stage::Renew, 1);
    let before = f.subs.active[0].clone();
    f.provider.fail_renew.store(true, Ordering::Release);
    let _ = f.start();
    wait_for(|| f.provider.gate.entered.load(Ordering::Acquire) == 1);
    f.stop.store(true, Ordering::Release);
    assert!(f.subs.poll_renewals(true).is_empty());
    assert_eq!(f.subs.maintenance.jobs.len(), 1);
    f.release();
    wait_for(|| {
        assert!(f.subs.poll_renewals(true).is_empty());
        f.subs.maintenance.jobs.is_empty()
    });
    assert_eq!(f.provider.subscriptions.load(Ordering::Relaxed), 1);
    assert_eq!(f.subs.active[0].renew_at, before.renew_at);
    f.finish();
}

#[test]
fn replacement_cannot_replay_an_initial_older_than_an_accepted_old_stream_event() {
    for generation in [1, 2] {
        let mut f = Fixture::new(generation, Stage::Subscribe, 1);
        f.provider.fail_renew.store(true, Ordering::Release);
        let old = f.subs.active[0].sid.clone();
        let player = f.wants[0].player.clone();
        let _ = f.start();
        wait_for(|| f.provider.gate.entered.load(Ordering::Acquire) == 1);
        let initial = f.sink.recv_or_closed(WAIT).unwrap().unwrap();
        assert_ne!(initial.sid, old);
        assert_eq!(initial.seq, 0);
        assert!(f.subs.hold_early(&initial));

        set_volume(&*f.provider.inner, f.wants[0].host, 43).unwrap();
        let mut playback = Playback::default();
        let mut old_seen = false;
        let mut new_dropped = false;
        while !old_seen || !new_dropped {
            let n = f.sink.recv_or_closed(WAIT).unwrap().unwrap();
            if n.sid == old {
                f.accept_volume(&mut playback, &n, 43);
                old_seen = true;
            } else {
                assert_eq!(n.sid, initial.sid);
                assert_eq!(n.seq, 1);
                // Lose the new stream's delta: its held initial is now older
                // than the volume the live model accepted from the old SID.
                new_dropped = true;
            }
        }
        let current = f.subs.active[0].clone();
        assert_eq!(playback.of(&player).unwrap().volume, Some(43));
        let released = Instant::now();
        f.release();
        assert!(f.drain().is_empty());
        let retained = &f.subs.active[0];
        assert_eq!(retained.sid, old);
        assert_eq!(retained.last_seq, current.last_seq);
        assert_eq!(retained.initial_deadline, current.initial_deadline);
        assert_eq!(retained.initial_misses, current.initial_misses);
        assert!(Arc::ptr_eq(&retained.epoch, &current.epoch));
        assert!(retained.refresh_pending);
        assert!(retained.renew_at >= released + RETRY);
        assert!(f.subs.take_early().is_none());
        assert_eq!(f.subs.maintenance.early_bytes, 0);
        assert!(f.subs.route(&initial).is_none());
        assert!(f.subs.route(&notify(&initial.sid, 2)).is_none());
        assert_eq!(playback.of(&player).unwrap().volume, Some(43));
        assert_eq!(
            f.provider.retired.lock().unwrap().as_slice(),
            std::slice::from_ref(&initial.sid)
        );
        for _ in 0..100 {
            assert!(f.start().is_empty());
        }
        assert_eq!(f.provider.renewals.load(Ordering::Relaxed), 1);
        assert_eq!(f.provider.subscriptions.load(Ordering::Relaxed), 2);

        // Advance the retained lease's retry deadline without sleeping. The
        // retry must SUBSCRIBE for new full state, not renew the old stream.
        f.subs.active[0].renew_at = Instant::now();
        let _ = f.start();
        assert_eq!(
            f.drain().iter().map(|(r, _)| r.resubscribed).sum::<usize>(),
            1
        );
        assert_eq!(f.provider.renewals.load(Ordering::Relaxed), 1);
        assert_eq!(f.provider.subscriptions.load(Ordering::Relaxed), 3);
        let recovered = f.sink.recv_or_closed(WAIT).unwrap().unwrap();
        assert_ne!(recovered.sid, old);
        assert_ne!(recovered.sid, initial.sid);
        assert_eq!(recovered.seq, 0);
        f.accept_volume(&mut playback, &recovered, 43);
        set_volume(&*f.provider.inner, f.wants[0].host, 44).unwrap();
        let later = f.sink.recv_or_closed(WAIT).unwrap().unwrap();
        assert_eq!(later.sid, recovered.sid);
        assert_eq!(later.seq, 1);
        f.accept_volume(&mut playback, &later, 44);
        assert!(f.subs.route(&later).is_none());
        assert_eq!(playback.of(&player).unwrap().volume, Some(44));
        f.finish();
    }
}

#[test]
fn lease_admission_does_not_adopt_a_ready_result_ahead_of_buffered_callbacks() {
    for generation in [1, 2] {
        let mut f = Fixture::new(generation, Stage::Renew, 1);
        let before = f.subs.active[0].clone();
        let _ = f.start();
        wait_for(|| f.provider.gate.entered.load(Ordering::Acquire) == 1);
        f.release();
        // Establish that a real renewal result is ready for adoption, then
        // restore it without letting the test's barrier change model state.
        let mut completed = None;
        wait_for(|| {
            completed = f.subs.maintenance.jobs[0]
                .output
                .lock()
                .unwrap()
                .try_recv()
                .ok();
            completed.is_some()
        });
        let (send, receive) = sync_channel(1);
        assert!(send.send(completed.unwrap()).is_ok());
        f.subs.maintenance.jobs[0].output = Mutex::new(receive);
        set_volume(&*f.provider.inner, f.wants[0].host, 29).unwrap();
        wait_for(|| {
            f.sim.gena_log().iter().any(|entry| {
                matches!(
                    entry.event,
                    fsonos_sim::GenaEvent::Delivered {
                        seq: 1,
                        status: 200,
                        ..
                    }
                )
            })
        });
        let provider: Provider = f.provider.clone();
        for _ in 0..100 {
            let sink = &f.sink;
            f.subs.start_renewals(
                &provider,
                |service| sink.callback_url(service.tag()),
                &f.stop,
                Instant::now(),
            );
        }
        assert_eq!(f.subs.maintenance.jobs.len(), 1);
        assert!(!f.subs.maintenance.jobs[0].finalized);
        assert_eq!(f.subs.active[0].renew_at, before.renew_at);
        assert_eq!(f.provider.renewals.load(Ordering::Relaxed), 1);
        // Only the caller's explicit poll after callback intake may adopt.
        let event = f.sink.recv_or_closed(WAIT).unwrap().unwrap();
        assert_eq!(event.sid, before.sid);
        assert_eq!(event.seq, 1);
        assert!(!f.subs.missed(&event));
        let reports = f.drain();
        assert_eq!(reports.iter().map(|(r, _)| r.renewed).sum::<usize>(), 1);
        assert!(f.subs.active[0].renew_at > before.renew_at);
        assert_eq!(f.subs.active[0].last_seq, Some(1));
        f.finish();
    }
}

mod deferred;
