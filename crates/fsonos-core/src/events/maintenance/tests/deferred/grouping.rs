//! New roles and reboots use the same bounded workers as ordinary renewals.

use super::super::*;

fn deferred(f: &mut Fixture) {
    f.subs.defer_network();
    for active in &mut f.subs.active {
        active.renew_at = Instant::now() + Duration::from_secs(900);
    }
}

fn sync(f: &mut Fixture, wants: &[Want], now: Instant) -> Report {
    f.subs
        .sync(&*f.provider, wants, |s| f.sink.callback_url(s.tag()), now)
}

fn drive(f: &mut Fixture) -> Vec<(Report, Instant)> {
    let mut reports = Vec::new();
    wait_for(|| {
        reports.extend(f.start());
        f.subs.pending.is_empty()
            && f.subs.maintenance.jobs.is_empty()
            && f.subs.maintenance.retire.len() == 0
            && f.subs.maintenance.retirement_due().is_none()
    });
    reports
}

#[test]
fn changed_roles_queue_new_leases_and_retire_old_sids_without_blocking() {
    for generation in [1, 2] {
        let mut f = Fixture::new(generation, Stage::Subscribe, 2);
        deferred(&mut f);
        let old = f.subs.active[0].clone();
        let new = Want {
            service: Service::AvTransport,
            ..old.want.clone()
        };
        let wants = [new.clone(), f.wants[1].clone()];
        let at = Instant::now();
        let report = sync(&mut f, &wants, at);
        assert!(at.elapsed() < Duration::from_secs(1));
        assert_eq!(report.dropped, 1);
        assert_eq!(report.subscribed, 0, "queued is not installed");
        assert!(f.subs.route(&notify(&old.sid, 1)).is_none());
        assert_eq!(f.provider.subscriptions.load(Ordering::Relaxed), 2);
        assert!(f.provider.retired.lock().unwrap().is_empty());
        let _ = f.start();
        wait_for(|| f.provider.gate.entered.load(Ordering::Acquire) == 1);
        let initial = f.sink.recv_or_closed(WAIT).unwrap().unwrap();
        assert_eq!(initial.seq, 0);
        assert!(f.subs.hold_early(&initial));
        for _ in 0..100 {
            assert_eq!(sync(&mut f, &wants, at), Report::default());
            assert!(
                !f.subs.maintenance.jobs[0]
                    .cancelled
                    .load(Ordering::Acquire)
            );
        }
        assert_eq!(f.subs.pending.len(), 1);
        assert_eq!(f.provider.subscriptions.load(Ordering::Relaxed), 3);
        assert!(f.provider.retired.lock().unwrap().is_empty());
        set_volume(&*f.provider.inner, f.wants[1].host, 59).unwrap();
        let other = f.sink.recv_or_closed(WAIT).unwrap().unwrap();
        assert_eq!(f.subs.route(&other).unwrap().0, &f.wants[1].player);
        assert!(!f.subs.missed(&other));
        f.release();
        let reports = drive(&mut f);
        assert_eq!(reports.iter().map(|(r, _)| r.subscribed).sum::<usize>(), 1);
        let replay = f.subs.take_early().unwrap();
        assert_eq!(replay, initial);
        assert_eq!(f.subs.route(&replay), Some((&new.player, new.service)));
        assert!(!f.subs.missed(&replay));
        assert!(f.subs.take_early().is_none());
        assert!(f.subs.route(&other).is_none());
        assert_eq!(f.provider.retired.lock().unwrap().as_slice(), [old.sid]);
        f.finish();
    }
}

#[test]
fn removed_and_readded_requests_cannot_adopt_an_old_subscribe_response() {
    let mut f = Fixture::new(2, Stage::Subscribe, 1);
    deferred(&mut f);
    let new = Want {
        service: Service::AvTransport,
        ..f.wants[0].clone()
    };
    let at = Instant::now();
    let _ = sync(&mut f, std::slice::from_ref(&new), at);
    let _ = f.start();
    wait_for(|| f.provider.gate.entered.load(Ordering::Acquire) == 1);
    let obsolete = f.sink.recv_or_closed(WAIT).unwrap().unwrap();
    let _ = sync(&mut f, &[], at);
    let _ = sync(&mut f, std::slice::from_ref(&new), at);
    // Same Want, same deadline, same pending epoch: cancellation is permanent.
    assert!(
        f.subs.maintenance.jobs[0]
            .cancelled
            .load(Ordering::Acquire)
    );
    for _ in 0..20 {
        let _ = f.start();
    }
    assert_eq!(f.provider.subscriptions.load(Ordering::Relaxed), 2);
    f.release();
    let reports = drive(&mut f);
    assert_eq!(reports.iter().map(|(r, _)| r.subscribed).sum::<usize>(), 1);
    assert_eq!(f.provider.subscriptions.load(Ordering::Relaxed), 3);
    assert!(f.subs.route(&obsolete).is_none());
    assert!(f.provider.retired.lock().unwrap().contains(&obsolete.sid));
    assert_eq!(f.subs.active.len(), 1);
    assert_ne!(f.subs.active[0].sid, obsolete.sid);
    f.finish();
}

#[test]
fn a_reboot_forgets_old_sids_and_queues_replacement_behind_an_inflight_renewal() {
    for generation in [1, 2] {
        let mut f = Fixture::new(generation, Stage::Renew, 2);
        deferred(&mut f);
        f.subs.active[0].renew_at = Instant::now();
        let old = f.subs.active[0].clone();
        let _ = f.start();
        wait_for(|| f.provider.gate.entered.load(Ordering::Acquire) == 1);
        let at = Instant::now();
        let report = f.subs.resubscribe(
            &*f.provider,
            &old.want.player,
            |s| f.sink.callback_url(s.tag()),
            at,
        );
        assert!(at.elapsed() < Duration::from_secs(1));
        assert_eq!(report, Report::default());
        assert!(f.subs.route(&notify(&old.sid, 1)).is_none());
        assert!(
            f.subs.maintenance.jobs[0]
                .cancelled
                .load(Ordering::Acquire)
        );
        let wants = f.wants.clone();
        for _ in 0..100 {
            let _ = sync(&mut f, &wants, at);
            let _ = f.start();
        }
        assert_eq!(f.provider.subscriptions.load(Ordering::Relaxed), 2);
        assert_eq!(f.provider.renewals.load(Ordering::Relaxed), 1);
        f.release();
        let reports = drive(&mut f);
        assert_eq!(reports.iter().map(|(r, _)| r.subscribed).sum::<usize>(), 1);
        assert_eq!(reports.iter().map(|(r, _)| r.renewed).sum::<usize>(), 0);
        assert!(f.provider.retired.lock().unwrap().is_empty());
        assert_eq!(f.subs.active.len(), 2);
        assert!(f.subs.active.iter().all(|a| a.sid != old.sid));
        f.finish();
    }
}

#[test]
fn unchanged_topology_preserves_failed_initial_subscription_backoff() {
    let mut f = Fixture::new(2, Stage::Subscribe, 1);
    deferred(&mut f);
    let player = f.wants[0].player.clone();
    let _ = f.subs.resubscribe(
        &*f.provider,
        &player,
        |s| f.sink.callback_url(s.tag()),
        Instant::now(),
    );
    f.provider.fail_subscribe.store(true, Ordering::Release);
    let _ = f.start();
    assert_eq!(f.drain()[0].0.failed.len(), 1);
    let due = f.subs.pending[0].1;
    let epoch = Arc::clone(&f.subs.pending_epoch);
    let wants = f.wants.clone();
    for _ in 0..100 {
        assert_eq!(sync(&mut f, &wants, Instant::now()), Report::default());
        let _ = f.start();
        assert_eq!(f.subs.pending[0].1, due);
        assert!(Arc::ptr_eq(&epoch, &f.subs.pending_epoch));
    }
    assert_eq!(f.provider.subscriptions.load(Ordering::Relaxed), 2);
    f.provider.fail_subscribe.store(false, Ordering::Release);
    f.subs.pending[0].1 = Instant::now();
    f.release();
    let reports = drive(&mut f);
    assert_eq!(reports.iter().map(|(r, _)| r.subscribed).sum::<usize>(), 1);
    assert_eq!(f.provider.subscriptions.load(Ordering::Relaxed), 3);
    f.finish();
}

#[test]
fn slow_retirement_shares_the_four_worker_limit_and_keeps_its_player_exclusion() {
    let mut f = Fixture::new(2, Stage::Cleanup, 5);
    deferred(&mut f);
    let mut expected: Vec<_> = f.subs.active.iter().map(|a| a.sid.clone()).collect();
    assert_eq!(sync(&mut f, &[], Instant::now()).dropped, 5);
    assert!(f.subs.is_empty());
    assert!(f.subs.next_due().is_some());
    let at = Instant::now();
    let _ = f.start();
    assert!(at.elapsed() < Duration::from_secs(1));
    wait_for(|| f.provider.gate.entered.load(Ordering::Acquire) == MAX_WORKERS);
    for _ in 0..100 {
        let _ = f.start();
    }
    assert_eq!(f.subs.maintenance.retire.len(), MAX_WORKERS);
    assert!(f.subs.maintenance.jobs.is_empty());
    assert_eq!(f.provider.gate.entered.load(Ordering::Acquire), MAX_WORKERS);
    f.release();
    assert!(drive(&mut f).is_empty());
    let mut retired = f.provider.retired.lock().unwrap().clone();
    retired.sort();
    expected.sort();
    assert_eq!(retired, expected);
    assert_eq!(f.subs.next_due(), None);
    f.finish();
}
