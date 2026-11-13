//! Moving one callback route must not restart another network's leases.

use super::*;

#[test]
fn a_route_move_fences_its_held_response_and_preserves_other_initials_and_active_sids() {
    for replacement in [false, true] {
        let mut f = Fixture::new(2, Stage::Subscribe, 3);
        deferred(&mut f);
        let moved = f.wants[0].clone();
        let old = f.subs.active[0].clone();
        let waiting = Want {
            service: Service::AvTransport,
            ..f.wants[1].clone()
        };
        let other_old = f.subs.active[1].sid.clone();
        let steady = f.subs.active[2].clone();
        let unchanged = [waiting.clone(), steady.want.clone()];
        let wanted = [moved.clone(), waiting.clone(), steady.want.clone()];
        let at = Instant::now();
        if !replacement {
            let _ = sync(&mut f, &unchanged, at);
        }
        let _ = sync(&mut f, &wanted, at);
        if replacement {
            let _ = f.subs.refresh(
                &*f.provider,
                &old.sid,
                |service| f.sink.callback_url(service.tag()),
                at,
            );
        }
        let epoch = Arc::clone(&f.subs.pending_epoch);
        let _ = f.start();
        wait_for(|| f.provider.gate.entered.load(Ordering::Acquire) == 2);
        let mut initials = (0..2)
            .map(|_| f.sink.recv_or_closed(WAIT).unwrap().unwrap())
            .collect::<Vec<_>>();
        let obsolete = initials.remove(
            initials
                .iter()
                .position(|event| event.path == "/RenderingControl")
                .unwrap(),
        );
        let waiting_initial = initials.pop().unwrap();
        assert_eq!(waiting_initial.path, "/AVTransport");
        assert!(f.subs.hold_early(&obsolete));
        assert!(f.subs.hold_early(&waiting_initial));

        // The live route switch removes only its host, then re-adds all
        // reachable roles. Use the exact same Want, Instant and pending epoch
        // to prove cancellation cannot be undone by a matching request.
        let _ = sync(&mut f, &unchanged, at);
        let _ = sync(&mut f, &wanted, at);
        assert!(Arc::ptr_eq(&epoch, &f.subs.pending_epoch));
        assert!(f.subs.pending.contains(&(waiting.clone(), at)));
        for job in &f.subs.maintenance.jobs {
            assert_eq!(
                job.cancelled.load(Ordering::Acquire),
                job.plan.want.player == moved.player,
            );
        }
        let preserved = f
            .subs
            .active
            .iter()
            .find(|a| a.want == steady.want)
            .unwrap();
        assert!(Arc::ptr_eq(&preserved.epoch, &steady.epoch));
        assert_eq!(preserved.sid, steady.sid);
        assert_eq!(preserved.renew_at, steady.renew_at);
        assert_eq!(preserved.last_seq, steady.last_seq);

        f.release();
        let reports = drive(&mut f);
        assert_eq!(reports.iter().map(|(r, _)| r.subscribed).sum::<usize>(), 2);
        assert_eq!(
            reports.iter().map(|(r, _)| r.resubscribed).sum::<usize>(),
            0
        );
        assert_eq!(f.provider.subscriptions.load(Ordering::Relaxed), 6);
        assert!(f.subs.route(&obsolete).is_none());
        assert_eq!(f.subs.take_early().unwrap(), waiting_initial);
        assert_eq!(
            f.subs.route(&waiting_initial),
            Some((&waiting.player, waiting.service)),
        );
        assert!(!f.subs.missed(&waiting_initial));
        assert!(f.subs.take_early().is_none());
        assert_eq!(
            f.subs.route(&notify(&steady.sid, 1)),
            Some((&steady.want.player, steady.want.service)),
        );
        let current = f.subs.active.iter().find(|a| a.want == moved).unwrap();
        assert_ne!(current.sid, old.sid);
        assert_ne!(current.sid, obsolete.sid);
        let mut retired = f.provider.retired.lock().unwrap().clone();
        retired.sort();
        let mut expected = vec![old.sid, other_old, obsolete.sid];
        expected.sort();
        assert_eq!(retired, expected);
        f.finish();
    }
}
