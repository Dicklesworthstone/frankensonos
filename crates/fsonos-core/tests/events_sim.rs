//! The event engine against `fsonos-sim` over real sockets: a routed
//! `net::Lan` subscribes every wanted service, the initial NOTIFYs route to
//! their players and fold into live state, and a rebooted player's
//! subscription is replaced when its renewal fails.

use fsonos_core::HouseholdState;
use fsonos_core::events::{self, Subscriptions, wanted};
use fsonos_core::playback::Playback;
use fsonos_proto::net::{EventSink, Lan};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

fn sim() -> SimHandle {
    SimHousehold::builder()
        .s1([
            SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1),
            SimPlayerSpec::new("Office", SimModel::Play5Gen1),
        ])
        .s2([SimPlayerSpec::new("Living Room", SimModel::One)])
        .spawn()
        .unwrap()
}

#[test]
fn every_service_subscribes_reports_in_and_survives_a_reboot() {
    let sim = sim();
    let lan = Lan::start()
        .unwrap()
        .with_routes(sim.players().iter().map(|p| (p.ip, p.addr)).collect());
    let mut houses: Vec<HouseholdState> = ["Kitchen", "Living Room"]
        .iter()
        .map(|room| {
            let mut st = HouseholdState::default();
            st.apply_topology(&get_zone_group_state(&lan, sim.player(room).unwrap().ip).unwrap());
            st
        })
        .collect();

    let local = lan
        .local_address_toward(sim.player("Kitchen").unwrap().ip)
        .unwrap();
    let sink = EventSink::start(SocketAddr::new(local, 0)).unwrap();
    let callback = |s: events::Service| sink.callback_url(s.tag());

    let want = wanted(&houses);
    let mut subs = Subscriptions::default();
    let t0 = Instant::now();
    let report = subs.sync(&lan, &want, callback, t0);
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert_eq!(report.subscribed, want.len());

    // Every subscription's initial full-state NOTIFY arrives, routes to its
    // player and service, and folds into the live state.
    let mut playback = Playback::default();
    let mut routed = 0;
    let deadline = Instant::now() + Duration::from_secs(10);
    while routed < want.len() && Instant::now() < deadline {
        let Some(n) = sink.recv_timeout(Duration::from_millis(200)) else {
            continue;
        };
        let Some((player, service)) = subs.route(&n).map(|(p, s)| (p.clone(), s)) else {
            continue;
        };
        events::apply(
            &mut houses,
            &mut playback,
            &player,
            service,
            &n,
            Instant::now(),
        )
        .unwrap();
        routed += 1;
    }
    assert_eq!(
        routed,
        want.len(),
        "an initial event from every subscription"
    );
    for w in want
        .iter()
        .filter(|w| w.service == events::Service::AvTransport)
    {
        assert!(
            playback.of(&w.player).and_then(|p| p.transport).is_some(),
            "{w:?}"
        );
    }
    for w in want
        .iter()
        .filter(|w| w.service == events::Service::RenderingControl)
    {
        assert!(
            playback.of(&w.player).and_then(|p| p.volume).is_some(),
            "{w:?}"
        );
    }

    // The Office reboots: its subscriptions are gone, so their renewals
    // fail (412) and the engine subscribes afresh.
    sim.reboot("Office", Duration::from_millis(100)).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let next = subs.next_due().unwrap();
    let renewed = subs.renew_due(&lan, callback, next);
    assert!(renewed.failed.is_empty(), "{:?}", renewed.failed);
    let office = want
        .iter()
        .filter(|w| {
            houses[0]
                .player(&w.player)
                .is_some_and(|p| p.room_name == "Office")
        })
        .count();
    assert!(office > 0);
    assert_eq!(
        renewed.resubscribed, office,
        "every Office subscription replaced"
    );
    assert_eq!(renewed.renewed, want.len() - office);
    assert_eq!(subs.len(), want.len());

    subs.unsubscribe_all(&lan);
}
