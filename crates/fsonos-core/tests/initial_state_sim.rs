//! Silent loss and repeated full-state recovery through real GENA sockets.
//! Explicit timer instants avoid sleeping through the watchdog's deadline.

use fsonos_core::HouseholdState;
use fsonos_core::events::{INITIAL_STATE_WAIT, RETRY, Service, wanted};
use fsonos_core::playback::Playback;
use fsonos_core::reconcile::Reconciler;
use fsonos_proto::control::{set_mute, set_volume};
use fsonos_proto::gena::Notify;
use fsonos_proto::net::{EventSink, Lan};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{GenaEvent, NotifyDrop, SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use fsonos_types::TransportState;
use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

fn household() -> SimHandle {
    SimHousehold::builder()
        .s1([SimPlayerSpec::new("Legacy", SimModel::Play5Gen1)])
        .s2([SimPlayerSpec::new("Modern", SimModel::One)])
        .spawn()
        .unwrap()
}

fn reconciler(lan: &Lan, host: IpAddr, now: Instant) -> Reconciler {
    let mut house = HouseholdState::default();
    house.apply_topology(&get_zone_group_state(lan, host).unwrap());
    let mut rec = Reconciler::new(RETRY, RETRY, now);
    rec.households.push(house);
    rec
}

fn listener(lan: &Lan, host: IpAddr) -> EventSink {
    let local = lan.local_address_toward(host).unwrap();
    EventSink::start(SocketAddr::new(local, 0)).unwrap()
}

fn receive(sink: &EventSink) -> Notify {
    sink.recv_timeout(Duration::from_secs(5))
        .expect("simulator did not deliver replacement initial state")
}

#[test]
fn repeated_initial_loss_recovers_all_services_without_a_delta_or_poll() {
    let sim = household();
    let lan = Lan::start()
        .unwrap()
        .with_routes(sim.players().iter().map(|p| (p.ip, p.addr)).collect());
    for room in ["Legacy", "Modern"] {
        let player = sim.player(room).unwrap();
        let host = player.ip;
        set_volume(&lan, host, 23).unwrap();
        set_mute(&lan, host, true).unwrap();
        let now = Instant::now();
        let mut rec = reconciler(&lan, host, now);
        let wants = wanted(&rec.households);
        assert_eq!(wants.len(), 4);
        let id = wants[0].player.clone();
        let sink = listener(&lan, host);
        let callback = |service: Service| sink.callback_url(service.tag());
        // Lose both the initial burst and the first replacement's burst.
        sim.drop_notifies(room, Some(NotifyDrop::Next(8))).unwrap();
        let subscribed = rec.subscriptions.sync(&lan, &wants, callback, now);
        assert_eq!(subscribed.subscribed, 4);
        assert!(subscribed.failed.is_empty());
        assert!(sink.recv_timeout(Duration::from_millis(50)).is_none());
        let calls = sim.soap_log().len();
        let mut playback = Playback::default();
        for attempt in 1..=2 {
            let due = now + INITIAL_STATE_WAIT * ((1 << attempt) - 1);
            assert_eq!(rec.subscriptions.next_due(), Some(due));
            let report = rec.subscriptions.renew_due(&lan, callback, due);
            assert_eq!(report.resubscribed, 4);
            assert_eq!(report.renewed, 0);
            assert!(report.failed.is_empty());
            assert!(report.answered.is_empty());
            assert_eq!(report.unanswered.len(), 4);
            if attempt == 1 {
                assert!(sink.recv_timeout(Duration::from_millis(50)).is_none());
                assert!(playback.of(&id).is_none());
            }
        }
        let due = now + INITIAL_STATE_WAIT * 3;
        let mut services = HashSet::new();
        for _ in 0..4 {
            let initial = receive(&sink);
            assert_eq!(initial.seq, 0);
            let (_, service) = rec.subscriptions.route(&initial).unwrap();
            assert!(services.insert(service));
            rec.on_notify(&lan, &mut playback, &initial, callback, due)
                .unwrap()
                .unwrap();
        }
        assert_eq!(services.len(), 4);
        let state = playback.of(&id).unwrap();
        assert_eq!(state.transport, Some(TransportState::Stopped));
        assert_eq!(state.volume, Some(23));
        assert_eq!(state.mute, Some(true));
        assert!(state.group_volume.is_some());
        assert!(state.group_mute.is_some());
        let dropped = sim
            .gena_log()
            .iter()
            .filter(|e| {
                e.player == player.uuid && matches!(e.event, GenaEvent::Dropped { seq: 0, .. })
            })
            .count();
        assert_eq!(dropped, 8);
        assert_eq!(
            sim.soap_log().len(),
            calls,
            "recovery must not poll or play"
        );
        let quiet = rec
            .subscriptions
            .renew_due(&lan, callback, due + INITIAL_STATE_WAIT);
        assert_eq!(quiet.resubscribed + quiet.renewed, 0);
        rec.subscriptions.unsubscribe_all(&lan);
    }
    sim.shutdown();
}

#[test]
fn a_lost_replacement_initial_repairs_stale_state_without_another_change() {
    let sim = household();
    let lan = Lan::start()
        .unwrap()
        .with_routes(sim.players().iter().map(|p| (p.ip, p.addr)).collect());
    for room in ["Legacy", "Modern"] {
        let host = sim.player(room).unwrap().ip;
        set_volume(&lan, host, 17).unwrap();
        set_mute(&lan, host, false).unwrap();
        let now = Instant::now();
        let mut rec = reconciler(&lan, host, now);
        let want = wanted(&rec.households)
            .into_iter()
            .find(|w| w.service == Service::RenderingControl)
            .unwrap();
        let id = want.player.clone();
        let sink = listener(&lan, host);
        let callback = |service: Service| sink.callback_url(service.tag());
        assert_eq!(
            rec.subscriptions
                .sync(&lan, &[want], callback, now)
                .subscribed,
            1
        );
        let mut playback = Playback::default();
        let original = receive(&sink);
        rec.on_notify(&lan, &mut playback, &original, callback, now)
            .unwrap()
            .unwrap();
        assert_eq!(playback.of(&id).unwrap().volume, Some(17));

        // Lose the volume delta; a mute delta exposes the sequence gap.
        sim.drop_notifies(room, Some(NotifyDrop::Next(1))).unwrap();
        set_volume(&lan, host, 41).unwrap();
        set_mute(&lan, host, true).unwrap();
        let gap = receive(&sink);
        assert_eq!(gap.sid, original.sid);
        assert_eq!(gap.seq, 2);
        // The gap recovery succeeds, but its full-state event is also lost.
        sim.drop_notifies(room, Some(NotifyDrop::Next(1))).unwrap();
        let report = rec
            .on_notify(&lan, &mut playback, &gap, callback, now)
            .unwrap()
            .unwrap();
        assert_eq!(report.events.resubscribed, 1);
        assert!(sink.recv_timeout(Duration::from_millis(50)).is_none());
        assert_eq!(playback.of(&id).unwrap().volume, Some(17));
        assert_eq!(playback.of(&id).unwrap().mute, Some(true));
        let calls = sim.soap_log().len();
        let due = now + INITIAL_STATE_WAIT;
        assert_eq!(rec.subscriptions.next_due(), Some(due));
        let repaired = rec.subscriptions.renew_due(&lan, callback, due);
        assert_eq!(repaired.resubscribed, 1);
        assert_eq!(repaired.renewed, 0);
        assert!(repaired.failed.is_empty());
        let fresh = receive(&sink);
        assert_ne!(fresh.sid, original.sid);
        assert_eq!(fresh.seq, 0);
        rec.on_notify(&lan, &mut playback, &fresh, callback, due)
            .unwrap()
            .unwrap();
        let state = playback.of(&id).unwrap().clone();
        assert_eq!(state.volume, Some(41));
        assert_eq!(state.mute, Some(true));
        assert!(
            rec.on_notify(&lan, &mut playback, &original, callback, due)
                .unwrap()
                .is_none()
        );
        assert_eq!(playback.of(&id), Some(&state));
        assert_eq!(sim.soap_log().len(), calls, "no additional SOAP commands");
        rec.subscriptions.unsubscribe_all(&lan);
    }
    sim.shutdown();
}
