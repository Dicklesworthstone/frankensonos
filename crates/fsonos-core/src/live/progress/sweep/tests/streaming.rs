//! Completed services must not wait for another service on the same player.

use super::*;
use std::sync::mpsc::{TryRecvError, sync_channel};

struct HeldStage {
    inner: Arc<dyn Transport + Send + Sync>,
    gate: Arc<Gate>,
    calls: AtomicUsize,
    hold_at: usize,
    panic_after: bool,
}

impl Transport for HeldStage {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let call = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
        let response = self.inner.soap_post(host, path, action, body)?;
        if call == self.hold_at {
            self.gate.entered.store(1, Ordering::Release);
            let deadline = Instant::now() + WAIT;
            while !self.gate.released.load(Ordering::Acquire) {
                if Instant::now() >= deadline {
                    return Err(ProtoError::NotWired("held service reply timed out"));
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            assert!(!self.panic_after, "injected service worker panic");
        }
        Ok(response)
    }
}

fn hold(f: &mut Fixture, hold_at: usize, panic_after: bool) -> (Arc<HeldStage>, Release) {
    let gate = Arc::new(Gate::default());
    let transport = Arc::new(HeldStage {
        inner: Arc::clone(&f.engine.transport),
        gate: Arc::clone(&gate),
        calls: AtomicUsize::new(0),
        hold_at,
        panic_after,
    });
    f.engine.transport = transport.clone();
    (transport, Release(gate))
}

#[test]
fn completed_services_publish_before_later_replies_on_both_generations() {
    for generation in [1, 2] {
        // The volume/mute pair uses calls 1-2, bracketed playback 3-7,
        // group volume 8 and transport settings 9. Hold real HTTP replies.
        for hold_at in [3, 8, 9] {
            let mut f = Fixture::new(generation, 0, false);
            let (transport, release) = hold(&mut f, hold_at, false);
            let (send, receive) = sync_channel(16);
            f.engine
                .subscribers
                .lock()
                .unwrap()
                .push(crate::live::delivery::Pending::new(send));
            let before = f.sim.soap_log().len();
            f.only_first();
            assert!(start_next(&f.engine, &mut f.model));
            wait_for(|| transport.gate.entered.load(Ordering::Acquire) == 1);
            let started = Instant::now();
            complete(&f.engine, &mut f.model);
            assert!(started.elapsed() < Duration::from_secs(1));
            let state = f.state(0).unwrap();
            assert_eq!(state.volume, Some(37));
            assert_eq!(state.mute, Some(true));
            assert_eq!(state.position_at(Instant::now()), (hold_at >= 8).then_some(41));
            assert_eq!(state.group_volume, (hold_at == 9).then_some(37));
            assert!(state.play_mode.is_none());
            assert_eq!(f.model.progress.sweep.len(), 1);
            assert!(!f.model.progress.sweep.pending[0].thread.is_finished());
            assert_eq!(
                f.engine.snapshot.lock().unwrap().playback.of(&f.players[0].0),
                Some(&state)
            );
            let changes = receive
                .try_iter()
                .filter(|event| matches!(event, LiveEvent::Playback { .. }))
                .count();
            assert_eq!(changes, if hold_at == 3 { 1 } else { hold_at - 6 });
            for _ in 0..20 {
                complete(&f.engine, &mut f.model);
            }
            assert_eq!(receive.try_recv(), Err(TryRecvError::Empty));
            assert_eq!(transport.calls.load(Ordering::Relaxed), hold_at);
            drop(release);
            f.drive();
            let state = f.state(0).unwrap();
            assert_eq!(state.position_at(Instant::now()), Some(41));
            // Accept our own later settings, despite the stamp advanced by
            // the already-published transport part of this very sweep.
            assert_eq!(state.play_mode.as_deref(), Some("NORMAL"));
            assert_eq!(state.group_volume, Some(37));
            let log = f.sim.soap_log();
            assert_eq!(log.len() - before, 9);
            assert!(log[before..].iter().all(|entry| entry.action.starts_with("Get")));
            f.finish();
        }
    }
}

#[test]
fn an_intervening_transport_event_fences_later_settings_but_a_volume_event_does_not() {
    for source in [EventSource::AvTransport, EventSource::RenderingControl] {
        let mut f = Fixture::new(2, 0, false);
        let (transport, release) = hold(&mut f, 8, false);
        f.only_first();
        assert!(start_next(&f.engine, &mut f.model));
        wait_for(|| transport.gate.entered.load(Ordering::Acquire) == 1);
        complete(&f.engine, &mut f.model);
        assert_eq!(f.state(0).unwrap().position_at(Instant::now()), Some(41));
        let body = match source {
            EventSource::AvTransport => "<CurrentPlayMode val=\"SHUFFLE_NOREPEAT\"/>",
            _ => "<Volume channel=\"Master\" val=\"59\"/>",
        };
        f.model
            .playback
            .apply(&f.players[0].0, source, &notify(source, body), Instant::now())
            .unwrap();
        drop(release);
        f.drive();
        let state = f.state(0).unwrap();
        let (mode, volume) = match source {
            EventSource::AvTransport => ("SHUFFLE_NOREPEAT", 37),
            _ => ("NORMAL", 59),
        };
        assert_eq!(state.play_mode.as_deref(), Some(mode));
        assert_eq!(state.volume, Some(volume));
        assert_eq!(state.group_volume, Some(37));
        f.finish();
    }
}

#[test]
fn a_newer_room_event_rejects_buffered_levels_without_discarding_transport() {
    let mut f = Fixture::new(2, 0, false);
    let (transport, release) = hold(&mut f, 3, false);
    f.only_first();
    assert!(start_next(&f.engine, &mut f.model));
    wait_for(|| transport.gate.entered.load(Ordering::Acquire) == 1);
    let source = EventSource::RenderingControl;
    let body = "<Volume channel=\"Master\" val=\"59\"/><Mute channel=\"Master\" val=\"0\"/>";
    f.model
        .playback
        .apply(&f.players[0].0, source, &notify(source, body), Instant::now())
        .unwrap();
    complete(&f.engine, &mut f.model);
    assert_eq!(f.state(0).unwrap().volume, Some(59));
    assert_eq!(f.state(0).unwrap().mute, Some(false));
    drop(release);
    f.drive();
    assert_eq!(f.state(0).unwrap().volume, Some(59));
    assert_eq!(f.state(0).unwrap().position_at(Instant::now()), Some(41));
    assert_eq!(f.state(0).unwrap().play_mode.as_deref(), Some("NORMAL"));
    f.finish();
}

#[test]
fn shutdown_after_partial_publication_keeps_the_worker_slot_until_exit() {
    let mut f = Fixture::new(2, 0, false);
    let (transport, release) = hold(&mut f, 8, false);
    f.only_first();
    assert!(start_next(&f.engine, &mut f.model));
    wait_for(|| transport.gate.entered.load(Ordering::Acquire) == 1);
    complete(&f.engine, &mut f.model);
    let before = f.state(0).unwrap();
    f.engine.stop.store(true, Ordering::Release);
    complete(&f.engine, &mut f.model);
    assert_eq!(f.model.progress.sweep.len(), 1);
    assert!(f.model.progress.sweep.pending[0].cancelled.load(Ordering::Acquire));
    drop(release);
    wait_for(|| {
        complete(&f.engine, &mut f.model);
        f.model.progress.sweep.len() == 0
    });
    assert_eq!(transport.calls.load(Ordering::Relaxed), 8);
    assert_eq!(f.state(0), Some(before));
    f.finish();
}

#[test]
fn a_panicking_later_service_does_not_lose_completed_services_or_leak_capacity() {
    let mut f = Fixture::new(2, 0, false);
    let (transport, release) = hold(&mut f, 8, true);
    f.only_first();
    assert!(start_next(&f.engine, &mut f.model));
    wait_for(|| transport.gate.entered.load(Ordering::Acquire) == 1);
    drop(release);
    wait_for(|| f.model.progress.sweep.pending[0].thread.is_finished());
    // Nothing was consumed before the worker panicked: its completed output
    // must survive independently of the JoinHandle's panic result.
    complete(&f.engine, &mut f.model);
    let state = f.state(0).unwrap();
    assert_eq!(state.volume, Some(37));
    assert_eq!(state.position_at(Instant::now()), Some(41));
    assert!(state.group_volume.is_none());
    assert!(state.play_mode.is_none());
    assert!(f.model.progress.sweep.pending.is_empty());
    assert!(f.model.progress.has_capacity());
    assert_eq!(transport.calls.load(Ordering::Relaxed), 8);
    f.finish();
}
