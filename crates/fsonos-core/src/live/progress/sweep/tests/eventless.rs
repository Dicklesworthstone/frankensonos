//! A callback outage must not turn periodic playback recovery synchronous.

use super::*;
use crate::live::{Live, progress};
use fsonos_proto::control::{pause, play};
use fsonos_proto::{Discovered, HttpBody};

#[test]
fn eventless_sweeps_keep_the_shared_worker_bound_and_do_not_add_polling() {
    for generation in [1, 2] {
        let mut f = Fixture::new(generation, 5, false);
        f.model.sink = None;
        f.begin();
        for _ in 0..20 {
            progress::recover(&f.engine, &mut f.model);
        }
        wait_for(|| f.transport.gate.entered.load(Ordering::Acquire) == 4);
        for _ in 0..100 {
            f.begin();
            progress::recover(&f.engine, &mut f.model);
        }
        assert_eq!(f.model.progress.sweep.len() + f.model.progress.reads.len(), 4);
        assert_eq!(f.transport.calls.load(Ordering::Relaxed), 4);
        assert!(f.model.progress.sweep.queue.len() <= f.players.len());
        assert!(f.model.sink.is_none());
        assert_eq!(f.model.rec.subscriptions.len(), 0);
        assert!(f.sim.gena_log().is_empty());
        f.release();
        f.drive();
        let calls = f.transport.calls.load(Ordering::Relaxed);
        assert_eq!(calls, 5 * 9);
        for _ in 0..20 {
            progress::recover(&f.engine, &mut f.model);
        }
        assert_eq!(f.transport.calls.load(Ordering::Relaxed), calls);
        for index in 0..f.players.len() {
            let state = f.state(index).unwrap();
            assert_eq!(state.volume, Some(37));
            assert_eq!(state.position_at(Instant::now()), Some(41));
        }
        f.finish();
    }
}

/// Leave discovery and bootstrap alone; hold one real room-level reply only
/// after readiness. All discovery stays confined to the simulator's routes.
struct StalledRoom {
    inner: Arc<dyn Transport + Send + Sync>,
    host: IpAddr,
    gate: Arc<Gate>,
    enabled: AtomicBool,
    slow_calls: AtomicUsize,
}

impl Transport for StalledRoom {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let active = self.enabled.load(Ordering::Acquire) && host == self.host;
        if active {
            self.slow_calls.fetch_add(1, Ordering::Relaxed);
        }
        let response = self.inner.soap_post(host, path, action, body)?;
        if active
            && action.trim_matches('"').ends_with("#GetVolume")
            && !self.gate.released.load(Ordering::Acquire)
        {
            self.gate.entered.fetch_add(1, Ordering::Release);
            let deadline = Instant::now() + WAIT;
            while !self.gate.released.load(Ordering::Acquire) {
                if Instant::now() >= deadline {
                    return Err(ProtoError::NotWired("eventless room read timed out"));
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        Ok(response)
    }

    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        self.inner.http_get(url)
    }

    fn http_get_bytes(&self, url: &str) -> Result<HttpBody, ProtoError> {
        self.inner.http_get_bytes(url)
    }

    fn discover(&self, mx_secs: u8, wait: Duration) -> Discovered {
        self.inner.discover(mx_secs, wait)
    }
}

fn running(generation: u8) -> (Fixture, Arc<StalledRoom>, Live, Release) {
    let f = Fixture::new(generation, 0, false);
    let gate = Arc::new(Gate::default());
    let transport = Arc::new(StalledRoom {
        inner: Arc::new(f.sim.lan()),
        host: f.players[0].1,
        gate: Arc::clone(&gate),
        enabled: AtomicBool::new(false),
        slow_calls: AtomicUsize::new(0),
    });
    let mut config = LiveConfig::new(f.players.iter().map(|(_, host)| *host).collect());
    // The fixture's unused callback listener holds the requested port open.
    // No synthetic callback error or fake household is injected into Live.
    config.callback_port = f.model.sink.as_ref().unwrap().local_addr().port();
    let live = Live::start_with(transport.clone(), Arc::clone(&f.engine.lan), config);
    let release = Release(gate);
    assert!(live.wait_ready(WAIT));
    let snapshot = live.snapshot();
    assert_eq!(snapshot.households.len(), 1);
    assert!(snapshot.callback.is_none());
    assert_eq!(snapshot.subscriptions, 0);
    assert!(snapshot.last_error.is_some());
    for (player, _) in &f.players {
        // Readiness must still include the synchronous bootstrap samples.
        let state = snapshot.playback.of(player).unwrap();
        assert_eq!(state.volume, Some(37));
        assert_eq!(state.position_at(Instant::now()), Some(41));
    }
    (f, transport, live, release)
}

#[test]
fn the_live_loop_recovers_healthy_rooms_and_stops_during_a_callback_outage() {
    for generation in [1, 2] {
        let (f, transport, live, release) = running(generation);
        let (healthy, host) = f.players[1].clone();
        set_volume(&f.sim.lan(), host, 59).unwrap();
        play(&f.sim.lan(), host).unwrap();
        pause(&f.sim.lan(), host).unwrap();
        let before = f.sim.soap_log().len();
        let events = live.subscribe();
        transport.enabled.store(true, Ordering::Release);
        live.refresh_soon();
        wait_for(|| transport.gate.entered.load(Ordering::Acquire) == 1);
        wait_for(|| {
            live.player(&healthy).is_some_and(|state| {
                state.volume == Some(59) && state.transport == Some(TransportState::Paused)
            })
        });
        assert!(!transport.gate.released.load(Ordering::Acquire));
        let snapshot = live.snapshot();
        assert!(snapshot.callback.is_none());
        assert_eq!(snapshot.subscriptions, 0);
        assert!(snapshot.last_error.is_some());
        assert!(f.sim.gena_log().is_empty());
        assert!(events.try_iter().any(|event| {
            matches!(event, LiveEvent::Playback { player, .. } if player == healthy)
        }));
        assert!(f.sim.soap_log()[before..].iter().all(|entry| entry.action.starts_with("Get")));
        let calls = transport.slow_calls.load(Ordering::Relaxed);
        let started = Instant::now();
        live.stop();
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(release);
        // Engine and workers release their transport handles on exit. The
        // held read observes stop before issuing any remaining service calls.
        wait_for(|| Arc::strong_count(&transport) == 1);
        assert_eq!(transport.slow_calls.load(Ordering::Relaxed), calls);
        f.finish();
    }
}

#[test]
fn callbacks_can_be_restored_while_an_eventless_read_is_still_pending() {
    for generation in [1, 2] {
        let (mut f, transport, live, release) = running(generation);
        transport.enabled.store(true, Ordering::Release);
        live.refresh_soon();
        wait_for(|| transport.gate.entered.load(Ordering::Acquire) == 1);
        drop(f.model.sink.take());
        live.refresh_soon();
        wait_for(|| {
            let snapshot = live.snapshot();
            snapshot.callback.is_some() && snapshot.subscriptions == 20
        });
        assert!(live.snapshot().last_error.is_none());
        set_volume(&f.sim.lan(), f.players[0].1, 59).unwrap();
        wait_for(|| live.player(&f.players[0].0).is_some_and(|state| state.volume == Some(59)));
        assert!(!transport.gate.released.load(Ordering::Acquire));
        assert_eq!(transport.gate.entered.load(Ordering::Acquire), 1);
        drop(release);
        // Only the running Engine and this test retain the wrapper once its
        // reads finish. Observe another two live turns to drain their output.
        wait_for(|| Arc::strong_count(&transport) == 2);
        std::thread::sleep(crate::live::TICK * 2);
        assert_eq!(live.player(&f.players[0].0).unwrap().volume, Some(59));
        live.stop();
        wait_for(|| Arc::strong_count(&transport) == 1);
        f.finish();
    }
}
