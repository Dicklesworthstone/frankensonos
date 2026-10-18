//! Releasing leases never puts old network calls back onto the live thread.

use super::*;
use crate::playback::{EventSource, Playback};

#[test]
fn released_leases_can_be_recreated_while_their_old_sids_wait_for_cleanup() {
    for generation in [1, 2] {
        let mut f = Fixture::new(generation, Stage::Cleanup, 2);
        deferred(&mut f);
        let mut old: Vec<_> = f.subs.active.iter().map(|a| a.sid.clone()).collect();
        let at = Instant::now();
        f.subs.release_background(f.provider.clone(), false);
        assert!(at.elapsed() < Duration::from_secs(1));
        assert!(f.subs.is_empty());
        assert!(f.subs.pending.is_empty());
        assert!(f.provider.retired.lock().unwrap().is_empty());
        assert_eq!(f.provider.subscriptions.load(Ordering::Relaxed), 2);
        for sid in &old {
            assert!(f.subs.route(&notify(sid, 1)).is_none());
        }
        let wants = f.wants.clone();
        assert_eq!(sync(&mut f, &wants, Instant::now()), Report::default());
        wait_for(|| {
            let _ = f.start();
            f.subs.len() == 2 && f.provider.gate.entered.load(Ordering::Acquire) == 2
        });
        assert_eq!(f.provider.subscriptions.load(Ordering::Relaxed), 4);
        assert!(!f.provider.gate.released.load(Ordering::Acquire));
        assert_eq!(f.subs.maintenance.retire.len(), 2);
        set_volume(&*f.provider.inner, f.wants[0].host, 59).unwrap();
        let mut playback = Playback::default();
        let deadline = Instant::now() + WAIT;
        loop {
            assert!(Instant::now() < deadline, "new callbacks did not arrive");
            let event = f.sink.recv_or_closed(WAIT).unwrap().unwrap();
            let Some((player, _)) = f.subs.route(&event) else {
                continue;
            };
            let player = player.clone();
            assert!(!old.contains(&event.sid));
            playback
                .apply(
                    &player,
                    EventSource::RenderingControl,
                    &event,
                    Instant::now(),
                )
                .unwrap();
            assert!(!f.subs.missed(&event));
            if playback
                .of(&f.wants[0].player)
                .is_some_and(|p| p.volume == Some(59))
            {
                break;
            }
        }
        assert!(f.provider.retired.lock().unwrap().is_empty());
        f.release();
        let _ = drive(&mut f);
        let mut retired = f.provider.retired.lock().unwrap().clone();
        old.sort();
        retired.sort();
        assert_eq!(retired, old);
        f.finish();
    }
}

#[test]
fn shutdown_hands_off_old_leases_without_waiting_or_exceeding_four_network_workers() {
    for generation in [1, 2] {
        let mut f = Fixture::new(generation, Stage::Cleanup, 5);
        deferred(&mut f);
        let mut expected: Vec<_> = f.subs.active.iter().map(|a| a.sid.clone()).collect();
        f.stop.store(true, Ordering::Release);
        let at = Instant::now();
        f.subs.release_background(f.provider.clone(), true);
        assert!(at.elapsed() < Duration::from_secs(1));
        assert!(f.subs.is_empty());
        assert!(f.subs.pending.is_empty());
        assert!(f.subs.maintenance.jobs.is_empty());
        assert_eq!(f.subs.maintenance.retire.len(), 0);
        assert_eq!(f.subs.next_due(), None);
        wait_for(|| f.provider.gate.entered.load(Ordering::Acquire) == MAX_WORKERS);
        for _ in 0..100 {
            let _ = f.subs.poll_renewals(true);
            assert_eq!(f.provider.gate.entered.load(Ordering::Acquire), MAX_WORKERS);
        }
        assert_eq!(f.provider.subscriptions.load(Ordering::Relaxed), 5);
        assert_eq!(f.provider.renewals.load(Ordering::Relaxed), 0);
        f.release();
        wait_for(|| Arc::strong_count(&f.provider) == 1);
        let mut retired = f.provider.retired.lock().unwrap().clone();
        expected.sort();
        retired.sort();
        assert_eq!(retired, expected);
        assert_eq!(f.provider.subscriptions.load(Ordering::Relaxed), 5);
        assert_eq!(f.provider.renewals.load(Ordering::Relaxed), 0);
        f.finish();
    }
}

#[test]
fn shutdown_cleans_unadopted_replacements_and_never_recreates_live_authority() {
    let mut f = Fixture::new(2, Stage::Subscribe, 1);
    deferred(&mut f);
    f.subs.active[0].renew_at = Instant::now();
    let old = f.subs.active[0].sid.clone();
    f.provider.fail_renew.store(true, Ordering::Release);
    let _ = f.start();
    wait_for(|| f.provider.gate.entered.load(Ordering::Acquire) == 1);
    let initial = f.sink.recv_or_closed(WAIT).unwrap().unwrap();
    assert_ne!(initial.sid, old);
    assert!(f.subs.hold_early(&initial));
    f.stop.store(true, Ordering::Release);
    let at = Instant::now();
    f.subs.release_background(f.provider.clone(), true);
    assert!(at.elapsed() < Duration::from_secs(1));
    assert!(f.subs.is_empty());
    assert!(f.subs.take_early().is_none());
    f.release();
    wait_for(|| Arc::strong_count(&f.provider) == 1);
    assert!(f.subs.is_empty());
    assert!(f.subs.route(&initial).is_none());
    assert_eq!(f.provider.subscriptions.load(Ordering::Relaxed), 2);
    assert_eq!(f.provider.renewals.load(Ordering::Relaxed), 1);
    let mut expected = vec![old, initial.sid];
    let mut retired = f.provider.retired.lock().unwrap().clone();
    expected.sort();
    retired.sort();
    assert_eq!(retired, expected);
    f.finish();
}
