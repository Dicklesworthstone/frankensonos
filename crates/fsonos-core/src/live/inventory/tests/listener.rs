//! Callback recovery has its own bounded retry, independent of discovery.

use super::*;
use std::net::{SocketAddr, TcpListener};

#[test]
fn an_occupied_callback_port_retries_without_waiting_for_held_discovery() {
    let mut f = Fixture::new(2, Stage::Discovery, false);
    let local = f.model.sink.as_ref().unwrap().local_addr().ip();
    let occupied = TcpListener::bind(SocketAddr::new(local, 0)).unwrap();
    f.engine.config.callback_port = occupied.local_addr().unwrap().port();
    f.engine.release(&mut f.model);
    f.model.sink = None;
    let mut surveys = Surveys::default();
    f.start(&mut surveys);
    let due = surveys.listener_after.unwrap();
    let calls = f.transport.calls.load(Ordering::Relaxed);
    assert!(f.model.last_error.is_some());
    assert_eq!(f.model.rec.subscriptions.len(), 0);
    drop(occupied);
    // Local bind failures have a fixed deadline; repeated live turns do not
    // spin or postpone that deadline even though discovery is still blocked.
    for _ in 0..100 {
        surveys.complete(&f.engine, &mut f.model);
        assert!(f.model.sink.is_none());
        assert_eq!(surveys.listener_after, Some(due));
    }
    surveys.refresh_soon();
    surveys.complete(&f.engine, &mut f.model);
    assert!(surveys.busy());
    assert_eq!(
        f.model.sink.as_ref().unwrap().local_addr().port(),
        f.engine.config.callback_port
    );
    assert_eq!(
        f.model.rec.subscriptions.len(),
        wanted(&f.model.rec.households).len()
    );
    assert!(f.model.last_error.is_none());
    let player = f.sim.player("B").unwrap();
    let id = PlayerId(player.uuid.clone());
    set_volume(&f.sim.lan(), player.ip, 59).unwrap();
    wait_for(|| {
        pump::turn(&f.engine, &mut f.model);
        surveys.complete(&f.engine, &mut f.model);
        f.model
            .playback
            .of(&id)
            .is_some_and(|p| p.volume == Some(59))
    });
    assert!(!f.transport.gate.released.load(Ordering::Acquire));
    assert_eq!(f.transport.calls.load(Ordering::Relaxed), calls);
    f.release();
    wait_for(|| {
        surveys.complete(&f.engine, &mut f.model);
        !surveys.busy()
    });
    assert!(f.model.rec.schedule.due(Instant::now()));
    f.finish(surveys);
}

#[test]
fn released_sleep_leases_return_on_the_existing_listener_with_or_without_a_survey() {
    for pending in [false, true] {
        let mut f = Fixture::new(2, Stage::Discovery, false);
        let mut surveys = Surveys::default();
        if pending {
            f.start(&mut surveys);
        }
        let address = f.model.sink.as_ref().unwrap().local_addr();
        let calls = f.transport.calls.load(Ordering::Relaxed);
        let count = wanted(&f.model.rec.households).len();
        f.engine.release(&mut f.model);
        assert_eq!(f.model.rec.subscriptions.len(), 0);
        surveys.complete(&f.engine, &mut f.model);
        assert_eq!(f.model.sink.as_ref().unwrap().local_addr(), address);
        assert_eq!(f.model.rec.subscriptions.len(), count);
        assert_eq!(surveys.busy(), pending);
        assert_eq!(f.transport.calls.load(Ordering::Relaxed), calls);
        assert_eq!(
            f.sim
                .gena_log()
                .iter()
                .filter(|e| matches!(e.event, GenaEvent::Subscribed { .. }))
                .count(),
            count * 2
        );
        if pending {
            assert!(
                surveys
                    .pending
                    .as_ref()
                    .unwrap()
                    .cancelled
                    .load(Ordering::Acquire)
            );
            f.release();
            wait_for(|| {
                surveys.complete(&f.engine, &mut f.model);
                !surveys.busy()
            });
        }
        assert!(f.model.rec.schedule.due(Instant::now()));
        f.finish(surveys);
    }
}

#[test]
fn real_live_callback_recovery_continues_while_discovery_is_blocked_on_both_generations() {
    for generation in [1, 2] {
        let sim = sim(generation);
        let lan = lan(&sim);
        let local = lan.local_address_toward(sim.players()[0].ip).unwrap();
        assert!(local.is_loopback());
        let occupied = TcpListener::bind(SocketAddr::new(local, 0)).unwrap();
        let transport = transport(&sim, Stage::Discovery);
        let release = Release(Arc::clone(&transport.gate));
        let mut config = LiveConfig::new(sim.players().iter().map(|p| p.ip).collect());
        config.callback_port = occupied.local_addr().unwrap().port();
        let live = Live::start_with(transport.clone(), lan, config);
        assert!(live.wait_ready(WAIT));
        assert!(live.snapshot().callback.is_none());
        assert_eq!(live.snapshot().subscriptions, 0);
        transport.gate.armed.store(true, Ordering::Release);
        live.refresh_soon();
        wait_for(|| transport.gate.entered.load(Ordering::Acquire));
        let calls = transport.calls.load(Ordering::Relaxed);
        let surveyed_at = live.snapshot().surveyed_at;
        drop(occupied);
        live.refresh_soon();
        let count = wanted(&live.households()).len();
        wait_for(|| {
            let snapshot = live.snapshot();
            snapshot.callback.is_some() && snapshot.subscriptions == count
        });
        assert!(live.snapshot().last_error.is_none());
        let player = sim.player("B").unwrap();
        let id = PlayerId(player.uuid.clone());
        set_volume(&sim.lan(), player.ip, 59).unwrap();
        wait_for(|| live.player(&id).is_some_and(|p| p.volume == Some(59)));
        assert!(!transport.gate.released.load(Ordering::Acquire));
        assert_eq!(live.snapshot().surveyed_at, surveyed_at);
        assert_eq!(transport.calls.load(Ordering::Relaxed), calls);
        let at = Instant::now();
        live.stop();
        assert!(at.elapsed() < Duration::from_secs(2));
        drop(release);
        wait_for(|| Arc::strong_count(&transport) == 1);
        assert_eq!(transport.calls.load(Ordering::Relaxed), calls);
        sim.shutdown();
    }
}

#[test]
fn clearing_the_listener_failure_does_not_hide_independent_discovery_failures() {
    let mut f = Fixture::new(2, Stage::Discovery, false);
    let mut surveys = Surveys::default();
    for (error, expected) in [
        ("listener failed", None),
        (
            "listener failed; survey failed: timeout",
            Some("survey failed: timeout"),
        ),
        ("separate failure", Some("separate failure")),
    ] {
        surveys.listener_error = Some("listener failed".into());
        f.model.last_error = Some(error.into());
        surveys.clear_listener_error(&mut f.model);
        assert_eq!(f.model.last_error.as_deref(), expected);
        assert!(surveys.listener_error.is_none());
    }
    f.finish(surveys);
}

mod leases;
