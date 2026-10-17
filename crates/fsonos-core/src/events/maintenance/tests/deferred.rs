//! Event parsing must never wait for a requested full-state replacement.

use super::*;
use crate::playback::Playback;
use crate::reconcile::Reconciler;

fn model(f: &mut Fixture) -> Reconciler {
    let now = Instant::now();
    let mut rec = Reconciler::new(RETRY, RETRY, now);
    rec.subscriptions = std::mem::take(&mut f.subs);
    let _ = rec.subscriptions.poll_renewals(false);
    for active in &mut rec.subscriptions.active {
        active.renew_at = now + Duration::from_secs(900);
    }
    rec
}

fn begin(f: &Fixture, rec: &mut Reconciler) {
    let provider: Provider = f.provider.clone();
    let _ = rec.subscriptions.renew_background(
        &provider,
        |s| f.sink.callback_url(s.tag()),
        &f.stop,
        Instant::now(),
    );
}

fn drain(rec: &mut Reconciler) -> Vec<(Report, Instant)> {
    let mut reports = Vec::new();
    wait_for(|| {
        reports.extend(rec.subscriptions.poll_renewals(false));
        rec.subscriptions.maintenance.jobs.is_empty()
    });
    reports
}

#[test]
fn sequence_gaps_request_full_state_without_waiting_and_keep_other_room_events_live() {
    for generation in [1, 2] {
        let mut f = Fixture::new(generation, Stage::Subscribe, 2);
        let mut rec = model(&mut f);
        let mut playback = Playback::default();
        let callback = |s: Service| f.sink.callback_url(s.tag());
        set_volume(&*f.provider.inner, f.wants[0].host, 13).unwrap();
        let lost = f.sink.recv_or_closed(WAIT).unwrap().unwrap();
        assert_eq!(lost.seq, 1);
        set_volume(&*f.provider.inner, f.wants[0].host, 17).unwrap();
        let gap = f.sink.recv_or_closed(WAIT).unwrap().unwrap();
        assert_eq!(gap.seq, 2);
        let at = Instant::now();
        let report = rec
            .on_notify(&*f.provider, &mut playback, &gap, callback, at)
            .unwrap()
            .unwrap();
        assert!(at.elapsed() < Duration::from_secs(1));
        assert_eq!(report.events.resubscribed, 0, "queued is not completed");
        assert_eq!(f.provider.subscriptions.load(Ordering::Relaxed), 2);
        assert_eq!(playback.of(&f.wants[0].player).unwrap().volume, Some(17));
        begin(&f, &mut rec);
        wait_for(|| f.provider.gate.entered.load(Ordering::Acquire) == 1);
        let initial = f.sink.recv_or_closed(WAIT).unwrap().unwrap();
        assert_ne!(initial.sid, gap.sid);
        assert_eq!(initial.seq, 0);
        assert!(rec.subscriptions.hold_early(&initial));
        set_volume(&*f.provider.inner, f.wants[1].host, 29).unwrap();
        let other = f.sink.recv_or_closed(WAIT).unwrap().unwrap();
        rec.on_notify(&*f.provider, &mut playback, &other, callback, Instant::now())
            .unwrap()
            .unwrap();
        assert_eq!(playback.of(&f.wants[1].player).unwrap().volume, Some(29));
        assert!(!f.provider.gate.released.load(Ordering::Acquire));
        f.release();
        let reports = drain(&mut rec);
        assert_eq!(reports.iter().map(|(r, _)| r.resubscribed).sum::<usize>(), 1);
        let initial = rec.subscriptions.take_early().unwrap();
        rec.on_notify(&*f.provider, &mut playback, &initial, callback, Instant::now())
            .unwrap()
            .unwrap();
        assert!(rec.subscriptions.take_early().is_none());
        assert!(rec.subscriptions.route(&gap).is_none());
        assert_eq!(f.provider.renewals.load(Ordering::Relaxed), 0);
        f.subs = rec.subscriptions;
        f.finish();
    }
}

#[test]
fn malformed_events_coalesce_and_corrected_same_sequence_survives_failed_recovery() {
    let mut f = Fixture::new(2, Stage::Subscribe, 1);
    let mut rec = model(&mut f);
    let mut playback = Playback::default();
    let callback = |s: Service| f.sink.callback_url(s.tag());
    let sid = rec.subscriptions.active[0].sid.clone();
    let mut bad = notify(&sid, 1);
    bad.properties.push(("LastChange".into(), "not-xml".into()));
    assert!(
        rec.on_notify(&*f.provider, &mut playback, &bad, callback, Instant::now())
            .is_err()
    );
    let due = rec.subscriptions.next_due();
    let epoch = Arc::clone(&rec.subscriptions.active[0].epoch);
    for _ in 0..100 {
        assert!(
            rec.on_notify(&*f.provider, &mut playback, &bad, callback, Instant::now())
                .is_err()
        );
        assert_eq!(rec.subscriptions.next_due(), due);
        assert!(Arc::ptr_eq(&epoch, &rec.subscriptions.active[0].epoch));
    }
    assert_eq!(f.provider.subscriptions.load(Ordering::Relaxed), 1);
    assert!(playback.of(&f.wants[0].player).is_none());
    f.provider.fail_subscribe.store(true, Ordering::Release);
    begin(&f, &mut rec);
    assert_eq!(drain(&mut rec)[0].0.failed.len(), 1);
    let backoff = rec.subscriptions.next_due();
    assert!(backoff.unwrap() > Instant::now() + RETRY / 2);
    set_volume(&*f.provider.inner, f.wants[0].host, 51).unwrap();
    let corrected = f.sink.recv_or_closed(WAIT).unwrap().unwrap();
    assert_eq!(corrected.seq, bad.seq);
    rec.on_notify(&*f.provider, &mut playback, &corrected, callback, Instant::now())
        .unwrap()
        .unwrap();
    assert!(rec.subscriptions.route(&corrected).is_none());
    assert_eq!(playback.of(&f.wants[0].player).unwrap().volume, Some(51));
    bad.seq = 2;
    for _ in 0..100 {
        assert!(
            rec.on_notify(&*f.provider, &mut playback, &bad, callback, Instant::now())
                .is_err()
        );
        begin(&f, &mut rec);
        assert_eq!(rec.subscriptions.next_due(), backoff);
    }
    assert_eq!(f.provider.subscriptions.load(Ordering::Relaxed), 2);
    f.provider.fail_subscribe.store(false, Ordering::Release);
    rec.subscriptions.active[0].renew_at = Instant::now();
    begin(&f, &mut rec);
    wait_for(|| f.provider.gate.entered.load(Ordering::Acquire) == 1);
    f.release();
    assert_eq!(drain(&mut rec)[0].0.resubscribed, 1);
    assert_eq!(f.provider.renewals.load(Ordering::Relaxed), 0);
    assert_eq!(f.provider.subscriptions.load(Ordering::Relaxed), 3);
    f.subs = rec.subscriptions;
    f.finish();
}

#[test]
fn a_gap_during_an_ordinary_renewal_keeps_its_replacement_request() {
    let mut f = Fixture::new(2, Stage::Renew, 1);
    let mut rec = model(&mut f);
    let mut playback = Playback::default();
    rec.subscriptions.active[0].renew_at = Instant::now();
    begin(&f, &mut rec);
    wait_for(|| f.provider.gate.entered.load(Ordering::Acquire) == 1);
    for volume in [11, 22] {
        set_volume(&*f.provider.inner, f.wants[0].host, volume).unwrap();
        let _lost = f.sink.recv_or_closed(WAIT).unwrap().unwrap();
    }
    set_volume(&*f.provider.inner, f.wants[0].host, 33).unwrap();
    let gap = f.sink.recv_or_closed(WAIT).unwrap().unwrap();
    assert_eq!(gap.seq, 3);
    rec.on_notify(
        &*f.provider,
        &mut playback,
        &gap,
        |s| f.sink.callback_url(s.tag()),
        Instant::now(),
    )
    .unwrap();
    assert!(rec.subscriptions.maintenance.jobs[0].cancelled.load(Ordering::Acquire));
    f.release();
    assert!(drain(&mut rec).is_empty(), "obsolete renewal was not adopted");
    assert!(rec.subscriptions.active[0].refresh_pending);
    assert_eq!(rec.subscriptions.active[0].last_seq, Some(3));
    begin(&f, &mut rec);
    assert_eq!(drain(&mut rec)[0].0.resubscribed, 1);
    assert_eq!(f.provider.subscriptions.load(Ordering::Relaxed), 2);
    assert_eq!(f.provider.renewals.load(Ordering::Relaxed), 1);
    f.subs = rec.subscriptions;
    f.finish();
}

#[test]
fn a_delayed_initial_cancels_silent_stream_replacement_even_when_renewal_was_due_first() {
    let mut f = Fixture::new(2, Stage::Subscribe, 1);
    let now = Instant::now();
    let active = &mut f.subs.active[0];
    let sid = active.sid.clone();
    active.last_seq = None;
    active.renew_at = now - Duration::from_secs(2);
    active.initial_deadline = now - Duration::from_secs(1);
    let _ = f.start();
    wait_for(|| f.provider.gate.entered.load(Ordering::Acquire) == 1);
    let replacement = f.sink.recv_or_closed(WAIT).unwrap().unwrap();
    assert_ne!(replacement.sid, sid);
    assert!(!f.subs.missed(&notify(&sid, 0)));
    assert!(f.subs.maintenance.jobs[0].cancelled.load(Ordering::Acquire));
    f.release();
    assert!(f.drain().is_empty());
    assert_eq!(f.subs.active[0].sid, sid);
    assert_eq!(f.subs.active[0].last_seq, Some(0));
    assert_eq!(
        f.provider.retired.lock().unwrap().as_slice(),
        [replacement.sid]
    );
    f.finish();
}

#[test]
fn dropping_the_owner_retires_unadopted_new_leases_without_joining() {
    let mut f = Fixture::new(2, Stage::Subscribe, 1);
    f.provider.fail_renew.store(true, Ordering::Release);
    let _ = f.start();
    wait_for(|| f.provider.gate.entered.load(Ordering::Acquire) == 1);
    let replacement = f.sink.recv_or_closed(WAIT).unwrap().unwrap();
    let at = Instant::now();
    drop(std::mem::take(&mut f.subs));
    assert!(at.elapsed() < Duration::from_secs(1));
    f.release();
    wait_for(|| Arc::strong_count(&f.provider) == 1);
    assert_eq!(
        f.provider.retired.lock().unwrap().as_slice(),
        [replacement.sid]
    );
    f.finish();
}
