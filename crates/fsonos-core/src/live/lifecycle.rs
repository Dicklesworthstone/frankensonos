//! A dead event loop must not retain a ready model and open event channels.

use super::{Engine, Snapshot, Subscribers};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

/// Owned independently of the engine so both ordinary return and unwinding
/// retire the externally visible liveness after its local resources unwind.
pub(super) struct Exit {
    snapshot: Arc<Mutex<Snapshot>>,
    subscribers: Subscribers,
    stop: Arc<AtomicBool>,
    ready: Arc<AtomicBool>,
}

impl Exit {
    pub(super) fn new(engine: &Engine, ready: &Arc<AtomicBool>) -> Self {
        Self {
            snapshot: Arc::clone(&engine.snapshot),
            subscribers: Arc::clone(&engine.subscribers),
            stop: Arc::clone(&engine.stop),
            ready: Arc::clone(ready),
        }
    }
}

impl Drop for Exit {
    fn drop(&mut self) {
        let requested = self.stop.swap(true, Ordering::AcqRel);
        self.ready.store(false, Ordering::Release);
        {
            let mut snapshot = self.snapshot.lock().unwrap_or_else(PoisonError::into_inner);
            snapshot.subscriptions = 0;
            snapshot.callback = None;
            snapshot.callbacks.clear();
            if std::thread::panicking() || !requested {
                // Panic payloads can contain runtime/site data. The panic hook
                // logs the original locally; public health needs only a cause.
                snapshot.last_error = Some(if std::thread::panicking() {
                    "the live-model worker panicked; restart the daemon".into()
                } else {
                    "the live-model worker stopped unexpectedly; restart the daemon".into()
                });
            }
        }
        self.subscribers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::{Live, LiveConfig};
    use fsonos_proto::net::Lan;
    use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
    use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
    use std::time::{Duration, Instant};

    fn fixture() -> (SimHandle, Arc<Lan>, LiveConfig) {
        let sim = SimHousehold::builder()
            .s2([SimPlayerSpec::new("Room", SimModel::One)])
            .spawn()
            .unwrap();
        let lan = Arc::new(
            Lan::start()
                .unwrap()
                .with_routes(sim.players().iter().map(|p| (p.ip, p.addr)).collect())
                .with_ssdp_target(sim.ssdp_addr()),
        );
        let config = LiveConfig::new(sim.players().iter().map(|p| p.ip).collect());
        (sim, lan, config)
    }

    fn wait_until(mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready() {
            assert!(Instant::now() < deadline, "worker did not finish in time");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn disconnected(receiver: &Receiver<super::super::LiveEvent>) {
        while receiver.try_recv().is_ok() {}
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Disconnected));
    }

    #[test]
    fn a_panicked_live_worker_retires_readiness_callbacks_and_subscriptions() {
        let (_sim, lan, mut config) = fixture();
        let fail = Arc::new(AtomicBool::new(false));
        let panic_now = Arc::clone(&fail);
        config.surveyed = Some(Arc::new(move |_| {
            assert!(
                !panic_now.load(Ordering::Acquire),
                "synthetic live-worker failure"
            );
        }));
        let mut live = Live::start(lan, config);
        let events = live.subscribe();
        assert!(live.wait_ready(Duration::from_secs(10)));
        assert!(!live.is_finished());
        let before = live.snapshot();
        assert!(before.callback.is_some());
        assert!(!before.households.is_empty());

        fail.store(true, Ordering::Release);
        live.refresh_soon();
        wait_until(|| live.is_finished());
        let snapshot = live.snapshot();
        assert_eq!(snapshot.households, before.households);
        assert_eq!(snapshot.subscriptions, 0);
        assert!(snapshot.callback.is_none());
        assert!(snapshot.callbacks.is_empty());
        assert_eq!(
            snapshot.last_error.as_deref(),
            Some("the live-model worker panicked; restart the daemon")
        );
        let began = Instant::now();
        assert!(!live.wait_ready(Duration::from_secs(30)));
        assert!(began.elapsed() < Duration::from_secs(1));
        disconnected(&events);
        assert_eq!(
            live.subscribe().recv_timeout(Duration::from_millis(50)),
            Err(RecvTimeoutError::Disconnected)
        );
        live.halt();
        assert!(
            live.is_finished(),
            "consuming the handle retains completion"
        );
    }

    #[test]
    fn an_initial_worker_panic_does_not_wait_out_the_startup_deadline() {
        let (_sim, lan, mut config) = fixture();
        config.surveyed = Some(Arc::new(|_| panic!("synthetic bootstrap failure")));
        let live = Live::start(lan, config);
        let events = live.subscribe();
        wait_until(|| live.is_finished());
        let began = Instant::now();
        assert!(!live.wait_ready(Duration::from_secs(30)));
        assert!(began.elapsed() < Duration::from_secs(1));
        disconnected(&events);
    }

    #[test]
    fn requested_shutdown_closes_streams_without_reporting_a_worker_failure() {
        let (_sim, lan, config) = fixture();
        let mut live = Live::start(lan, config);
        let events = live.subscribe();
        assert!(live.wait_ready(Duration::from_secs(10)));
        assert!(!live.is_finished());
        live.halt();
        assert!(live.is_finished());
        assert!(!live.wait_ready(Duration::ZERO));
        let snapshot = live.snapshot();
        assert_eq!(snapshot.subscriptions, 0);
        assert!(snapshot.callback.is_none());
        assert!(snapshot.callbacks.is_empty());
        assert!(snapshot.last_error.is_none(), "{:?}", snapshot.last_error);
        disconnected(&events);
    }
}
