//! Exercise renewal and full-state recovery over the real simulator HTTP/GENA path.

use fsonos_core::HouseholdState;
use fsonos_core::events::{RETRY, Service, wanted};
use fsonos_core::playback::Playback;
use fsonos_core::reconcile::Reconciler;
use fsonos_proto::control::{set_mute, set_volume};
use fsonos_proto::gena::Notify;
use fsonos_proto::net::{EventSink, Lan};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHousehold, SimModel, SimPlayerSpec};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

fn receive(sink: &EventSink) -> Notify {
    sink.recv_timeout(Duration::from_secs(5))
        .expect("simulator did not deliver its NOTIFY")
}

#[test]
fn real_subscriptions_recover_lost_and_malformed_volume_events() {
    let sim = SimHousehold::builder()
        .s1([SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1)])
        .spawn()
        .unwrap();
    let lan = Lan::start()
        .unwrap()
        .with_routes(sim.players().iter().map(|p| (p.ip, p.addr)).collect());
    let host = sim.player("Kitchen").unwrap().ip;
    set_volume(&lan, host, 17).unwrap();
    set_mute(&lan, host, false).unwrap();
    let mut house = HouseholdState::default();
    house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
    let now = Instant::now();
    let mut rec = Reconciler::new(RETRY, RETRY, now);
    rec.households.push(house);
    let want = wanted(&rec.households)
        .into_iter()
        .find(|w| w.service == Service::RenderingControl)
        .unwrap();
    let id = want.player.clone();
    let local = lan.local_address_toward(host).unwrap();
    let sink = EventSink::start(SocketAddr::new(local, 0)).unwrap();
    let callback = |service: Service| sink.callback_url(service.tag());
    let report = rec.subscriptions.sync(&lan, &[want], callback, now);
    assert_eq!(report.subscribed, 1);
    assert!(report.failed.is_empty());
    let mut playback = Playback::default();
    let initial = receive(&sink);
    assert_eq!(initial.seq, 0);
    rec.on_notify(&lan, &mut playback, &initial, callback, now)
        .unwrap()
        .unwrap();
    assert_eq!(playback.of(&id).unwrap().volume, Some(17));

    // A real renewal must not manufacture a gap on the next real delta.
    let at = rec.subscriptions.next_due().unwrap();
    let renewal = rec.subscriptions.renew_due(&lan, callback, at);
    assert_eq!(renewal.renewed, 1);
    set_volume(&lan, host, 29).unwrap();
    let next = receive(&sink);
    assert_eq!(next.sid, initial.sid);
    assert_eq!(next.seq, 1);
    let report = rec
        .on_notify(&lan, &mut playback, &next, callback, at)
        .unwrap()
        .unwrap();
    assert_eq!(report.events.resubscribed, 0);
    assert_eq!(playback.of(&id).unwrap().volume, Some(29));

    // Consume but do not apply one notification, then let the next expose it.
    set_volume(&lan, host, 41).unwrap();
    let lost = receive(&sink);
    assert_eq!(lost.seq, 2);
    set_mute(&lan, host, true).unwrap();
    let gap = receive(&sink);
    assert_eq!(gap.seq, 3);
    let report = rec
        .on_notify(&lan, &mut playback, &gap, callback, at)
        .unwrap()
        .unwrap();
    assert_eq!(report.events.resubscribed, 1);
    let fresh = receive(&sink);
    assert_ne!(fresh.sid, initial.sid);
    assert_eq!(fresh.seq, 0);
    rec.on_notify(&lan, &mut playback, &fresh, callback, at)
        .unwrap()
        .unwrap();
    assert_eq!(playback.of(&id).unwrap().volume, Some(41));
    assert_eq!(playback.of(&id).unwrap().mute, Some(true));
    let stale = rec
        .on_notify(&lan, &mut playback, &lost, callback, at)
        .unwrap();
    assert!(stale.is_none());

    // Corrupt the next delivered delta. The replacement must request the
    // simulator's actual full state, not merely renew the unreadable stream.
    set_volume(&lan, host, 55).unwrap();
    let mut malformed = receive(&sink);
    malformed
        .properties
        .iter_mut()
        .find(|(name, _)| name == "LastChange")
        .unwrap()
        .1 = "<Event>".into();
    assert!(
        rec.on_notify(&lan, &mut playback, &malformed, callback, at)
            .is_err()
    );
    assert_eq!(playback.of(&id).unwrap().volume, Some(41));
    let restored = receive(&sink);
    assert_ne!(restored.sid, fresh.sid);
    assert_eq!(restored.seq, 0);
    rec.on_notify(&lan, &mut playback, &restored, callback, at)
        .unwrap()
        .unwrap();
    assert_eq!(playback.of(&id).unwrap().volume, Some(55));
    assert_eq!(rec.subscriptions.len(), 1);
    rec.subscriptions.unsubscribe_all(&lan);
}
