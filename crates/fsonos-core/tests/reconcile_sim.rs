//! The reconcile loop against `fsonos-sim` over real sockets: a routed
//! `net::Lan` with the simulator's unicast SSDP responder surveys both
//! households, subscriptions follow the model, a player that goes offline
//! and comes back is dropped, marked, and recovered, and a reboot reported
//! by a topology event is resubscribed at once.

use fsonos_core::events::{self, Service};
use fsonos_core::playback::Playback;
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
    assert_eq!(
        third.rebooted,
        std::slice::from_ref(&office),
        "it booted on its way back"
    );
    assert_eq!(r.subscriptions.len(), all, "back to the full set");
    assert_eq!(r.health.of(&office).unwrap().health, Health::Healthy);

    r.subscriptions.unsubscribe_all(&lan);
}

#[test]
fn a_reboot_in_a_topology_event_resubscribes_and_refreshes_within_5_s() {
    let sim = SimHousehold::builder()
        .s1([
            SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1),
            SimPlayerSpec::new("Office", SimModel::Play5Gen1),
        ])
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
    let callback = |s: Service| sink.callback_url(s.tag());

    let t0 = Instant::now();
    let mut r = Reconciler::new(Duration::from_mins(5), Duration::from_mins(30), t0);
    r.refresh(&lan, &[], callback, t0).unwrap();
    let all = r.subscriptions.len();
    let mut playback = Playback::default();
    // Take in every initial NOTIFY; this also records each player's BootSeq.
    while let Some(n) = sink.recv_timeout(Duration::from_millis(500)) {
        r.on_notify(&lan, &mut playback, &n, callback, Instant::now())
            .unwrap();
    }

    // Reboot the player that does not carry the household's topology
    // subscription, so that subscription reports the new BootSeq.
    let want = events::wanted(&r.households);
    let topology_host = &want
        .iter()
        .find(|w| w.service == Service::ZoneGroupTopology)
        .unwrap()
        .player;
    let victim = r.households[0]
        .players
        .iter()
        .find(|p| &p.id != topology_host)
        .unwrap();
    let (victim_id, victim_room) = (victim.id.clone(), victim.room_name.clone());
    let victim_wants = want.iter().filter(|w| w.player == victim_id).count();
    assert!(victim_wants > 0);
    sim.reboot(&victim_room, Duration::ZERO).unwrap();

    let started = Instant::now();
    let mut rebooted = Vec::new();
    let mut resubscribed = 0;
    let mut fresh = 0;
    while fresh < victim_wants && started.elapsed() < Duration::from_secs(5) {
        let Some(n) = sink.recv_timeout(Duration::from_millis(200)) else {
            continue;
        };
        let Some(report) = r
            .on_notify(&lan, &mut playback, &n, callback, Instant::now())
            .unwrap()
        else {
            continue;
        };
        if !report.rebooted.is_empty() {
            assert!(
                report.events.failed.is_empty(),
                "{:?}",
                report.events.failed
            );
            rebooted = report.rebooted;
            resubscribed = report.events.resubscribed;
        } else if !rebooted.is_empty()
            && n.seq == 0
            && r.subscriptions.route(&n).map(|(p, _)| p) == Some(&victim_id)
        {
            fresh += 1;
        }
    }
    assert_eq!(
        rebooted,
        std::slice::from_ref(&victim_id),
        "the topology event showed the reboot"
    );
    assert_eq!(resubscribed, victim_wants);
    assert_eq!(
        fresh, victim_wants,
        "every fresh subscription delivered its full-state NOTIFY within 5 s"
    );
    assert_eq!(r.subscriptions.len(), all);
    assert_eq!(r.health.of(&victim_id).unwrap().health, Health::Healthy);

    // The replacements are live: renewing everything (all are due by a full
    // timeout from now) succeeds without the 412 fallback.
    let later = Instant::now() + Duration::from_secs(u64::from(events::TIMEOUT_SECS));
    let renewed = r.subscriptions.renew_due(&lan, callback, later);
    assert_eq!((renewed.renewed, renewed.resubscribed), (all, 0));
    r.subscriptions.unsubscribe_all(&lan);
}

#[test]
fn a_survey_catches_a_reboot_the_events_missed() {
    let sim = SimHousehold::builder()
        .s1([
            SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1),
            SimPlayerSpec::new("Office", SimModel::Play5Gen1),
        ])
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
    let callback = |s: Service| sink.callback_url(s.tag());

    let t0 = Instant::now();
    let interval = Duration::from_mins(5);
    let mut r = Reconciler::new(interval, Duration::from_mins(30), t0);
    let first = r.refresh(&lan, &[], callback, t0).unwrap();
    assert!(first.rebooted.is_empty(), "first sighting only records");
    let all = r.subscriptions.len();
    let office = r.households[0]
        .players
        .iter()
        .find(|p| p.room_name == "Office")
        .unwrap()
        .id
        .clone();
    let office_wants = events::wanted(&r.households)
        .iter()
        .filter(|w| w.player == office)
        .count();

    // No NOTIFY is processed here, so only the next survey can see the reboot.
    sim.reboot("Office", Duration::ZERO).unwrap();
    let second = r.refresh(&lan, &[], callback, t0 + interval).unwrap();
    assert_eq!(second.rebooted, std::slice::from_ref(&office));
    assert!(
        second.events.failed.is_empty(),
        "{:?}",
        second.events.failed
    );
    assert_eq!(second.events.subscribed, office_wants, "subscribed afresh");
    assert_eq!(r.subscriptions.len(), all);

    // No stale SID is left: renewing everything succeeds without the 412
    // fallback.
    let later = Instant::now() + Duration::from_secs(u64::from(events::TIMEOUT_SECS));
    let renewed = r.subscriptions.renew_due(&lan, callback, later);
    assert_eq!((renewed.renewed, renewed.resubscribed), (all, 0));
    r.subscriptions.unsubscribe_all(&lan);
}
