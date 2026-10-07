//! The reconcile loop against `fsonos-sim` over real sockets: a routed
//! `net::Lan` with the simulator's unicast SSDP responder surveys both
//! households, subscriptions follow the model, and a player that goes
//! offline and comes back is dropped, marked, and recovered.

use fsonos_core::reconcile::{Health, Reconciler};
use fsonos_proto::net::{EventSink, Lan};
use fsonos_sim::{SimHousehold, SimModel, SimPlayerSpec};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

#[test]
fn refresh_follows_players_going_offline_and_coming_back() {
    let sim = SimHousehold::builder()
        .s1([
            SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1),
            SimPlayerSpec::new("Office", SimModel::Play5Gen1),
        ])
        .s2([SimPlayerSpec::new("Living Room", SimModel::One)])
        .spawn()
        .unwrap();
    let lan = Lan::start()
        .unwrap()
        .with_routes(sim.players().iter().map(|p| (p.ip, p.addr)).collect())
        .with_ssdp_target(sim.ssdp_addr());
    let local = lan
        .local_address_toward(sim.player("Kitchen").unwrap().ip)
        .unwrap();
    let sink = EventSink::start(SocketAddr::new(local, 0)).unwrap();
    let callback = |s: fsonos_core::events::Service| sink.callback_url(s.tag());

    let t0 = Instant::now();
    let interval = Duration::from_mins(5);
    let mut r = Reconciler::new(interval, Duration::from_mins(30), t0);
    let first = r.refresh(&lan, &[], callback, t0).unwrap();
    assert_eq!((first.households, first.players), (2, 3));
    assert!(first.missing.is_empty());
    assert!(first.events.failed.is_empty(), "{:?}", first.events.failed);
    let all = r.subscriptions.len();
    assert_eq!(first.events.subscribed, all);
    assert!(!r.schedule.due(t0 + Duration::from_secs(1)));

    let office = r
        .households
        .iter()
        .flat_map(|h| &h.players)
        .find(|p| p.room_name == "Office")
        .unwrap()
        .id
        .clone();

    // The Office drops off the network: the next survey misses it, its
    // subscriptions go, and it is marked offline.
    sim.set_offline("Office", true).unwrap();
    let t1 = t0 + interval;
    let second = r.refresh(&lan, &[], callback, t1).unwrap();
    assert_eq!(second.missing, std::slice::from_ref(&office));
    assert_eq!(second.players, 2);
    assert!(
        second.events.dropped > 0,
        "the Office's subscriptions were dropped"
    );
    assert!(r.subscriptions.len() < all);
    assert_eq!(r.health.of(&office).unwrap().health, Health::Offline);

    // It comes back: the next survey finds it, resubscribes, and it is healthy.
    sim.set_offline("Office", false).unwrap();
    let t2 = t1 + interval;
    let third = r.refresh(&lan, &[], callback, t2).unwrap();
    assert_eq!(third.players, 3);
    assert!(third.missing.is_empty());
    assert_eq!(r.subscriptions.len(), all, "back to the full set");
    assert_eq!(r.health.of(&office).unwrap().health, Health::Healthy);

    r.subscriptions.unsubscribe_all(&lan);
}
