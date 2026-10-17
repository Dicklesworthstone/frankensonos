//! The doctor's LAN checks against `fsonos-sim` through the real LAN
//! transport (routed to the virtual players' loopback sockets), healthy and
//! with faults: swallowed NOTIFYs and a player that went offline.

use fsonos_core::doctor::lan::{self, LanProbe, Neighbors};
use fsonos_core::doctor::{EXIT_FAIL, EXIT_OK, EXIT_WARN, Report, Runner, Status};
use fsonos_proto::net::Lan;
use fsonos_sim::{GenaEvent, NotifyDrop, SimHandle, SimHousehold};
use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

fn lan_for(sim: &SimHandle) -> Arc<Lan> {
    let routes = sim.players().iter().map(|p| (p.ip, p.addr)).collect();
    Arc::new(
        Lan::start()
            .unwrap()
            .with_routes(routes)
            .with_ssdp_target(sim.ssdp_addr()),
    )
}

/// A routed Lan whose SSDP goes to a socket that never answers, like
/// multicast that does not reach the players (routed subnets, Wi-Fi that
/// filters it).
fn lan_without_ssdp(sim: &SimHandle, silent: &UdpSocket) -> Arc<Lan> {
    let routes = sim.players().iter().map(|p| (p.ip, p.addr)).collect();
    Arc::new(
        Lan::start()
            .unwrap()
            .with_routes(routes)
            .with_ssdp_target(silent.local_addr().unwrap()),
    )
}

fn doctor(sim: &SimHandle, seeds: Vec<IpAddr>) -> Report {
    doctor_on(lan_for(sim), seeds)
}

fn doctor_on(lan: Arc<Lan>, seeds: Vec<IpAddr>) -> Report {
    doctor_with(lan, seeds, lan::system_neighbors)
}

/// [`doctor_on`], reading the neighbor table with `neighbors`.
fn doctor_with(lan: Arc<Lan>, seeds: Vec<IpAddr>, neighbors: fn() -> Option<Neighbors>) -> Report {
    let dir = tempfile::tempdir().unwrap();
    let probe = Arc::new(
        LanProbe::new(lan, seeds)
            .with_ssdp_wait(Duration::from_secs(1))
            .with_neighbors(neighbors),
    );
    let mut runner = Runner::new();
    lan::register(&mut runner, dir.path().join("data"), &probe);
    let report = runner.run().unwrap();
    eprintln!("{}", report.render_table());
    report
}

fn status(report: &Report, id: fsonos_core::doctor::CheckId) -> Status {
    report.get(id).unwrap_or_else(|| panic!("{id} ran")).status
}

#[test]
fn a_healthy_house_passes() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let report = doctor(&sim, Vec::new());
    for id in [
        lan::STORE_OPEN,
        lan::SSDP,
        lan::PLAYERS,
        lan::HOUSEHOLDS,
        lan::GENA_ROUNDTRIP,
        lan::TOPOLOGY,
    ] {
        assert_eq!(
            status(&report, id),
            Status::Pass,
            "{id}: {:?}",
            report.get(id)
        );
    }
    assert_eq!(status(&report, lan::SEEDS), Status::Skip);
    // The sim offers no software update unless asked to.
    assert_eq!(status(&report, lan::UPDATES), Status::Skip);
    assert_eq!(report.exit_code(), EXIT_OK);
    let ssdp = report.get(lan::SSDP).unwrap();
    assert!(
        ssdp.summary.ends_with("in 2 household(s)"),
        "{}",
        ssdp.summary
    );
    sim.shutdown();
}

#[test]
fn swallowed_notifies_fail_the_gena_round_trip() {
    let sim = SimHousehold::standard().spawn().unwrap();
    for p in sim.players() {
        sim.drop_notifies(&p.uuid, Some(NotifyDrop::Next(100)))
            .unwrap();
    }
    let report = doctor(&sim, Vec::new());
    let gena = report.get(lan::GENA_ROUNDTRIP).unwrap();
    assert_eq!(gena.status, Status::Fail, "{gena:?}");
    assert!(
        gena.summary.contains("no event reached"),
        "{}",
        gena.summary
    );
    assert!(gena.remedy.as_deref().unwrap().contains("Firewall"));
    // Everything else still works.
    assert_eq!(status(&report, lan::PLAYERS), Status::Pass);
    assert_eq!(report.exit_code(), EXIT_FAIL);
    sim.shutdown();
}

#[test]
fn an_offline_seeded_player_fails_seeds_and_players() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let office = sim.player("Office").unwrap().clone();
    sim.set_offline(&office.uuid, true).unwrap();
    let report = doctor(&sim, vec![office.ip]);
    let seeds = report.get(lan::SEEDS).unwrap();
    assert_eq!(seeds.status, Status::Fail, "{seeds:?}");
    assert!(
        seeds
            .detail
            .as_deref()
            .unwrap()
            .starts_with(&office.ip.to_string())
    );
    let players = report.get(lan::PLAYERS).unwrap();
    assert_eq!(players.status, Status::Fail, "{players:?}");
    assert!(
        players
            .detail
            .as_deref()
            .unwrap()
            .contains(&office.ip.to_string())
    );
    assert_eq!(report.exit_code(), EXIT_FAIL);
    sim.shutdown();
}

#[test]
fn the_event_round_trip_ends_its_subscription() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let report = doctor(&sim, Vec::new());
    assert_eq!(status(&report, lan::GENA_ROUNDTRIP), Status::Pass);
    let log = sim.gena_log();
    let count = |service: Option<&str>, is: fn(&GenaEvent) -> bool| {
        log.iter()
            .filter(|e| service.is_none_or(|s| e.service == s) && is(&e.event))
            .count()
    };
    let subscribed = |e: &GenaEvent| matches!(e, GenaEvent::Subscribed { .. });
    let unsubscribed = |e: &GenaEvent| matches!(e, GenaEvent::Unsubscribed { .. });
    // The round trip's own subscription, then every one the doctor made
    // (lan.updates reads each household's topology event too).
    let rc = Some("RenderingControl");
    assert_eq!(
        (count(rc, subscribed), count(rc, unsubscribed)),
        (1, 1),
        "{log:?}"
    );
    assert_eq!(
        count(None, subscribed),
        count(None, unsubscribed),
        "{log:?}"
    );
}

#[test]
fn dead_ssdp_with_working_seeds_warns_and_everything_still_runs() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let silent = UdpSocket::bind("127.0.0.1:0").unwrap();
    let seeds: Vec<IpAddr> = sim.players().iter().map(|p| p.ip).collect();
    let report = doctor_on(lan_without_ssdp(&sim, &silent), seeds);
    let ssdp = report.get(lan::SSDP).unwrap();
    assert_eq!(ssdp.status, Status::Warn, "{ssdp:?}");
    for id in [
        lan::SEEDS,
        lan::PLAYERS,
        lan::HOUSEHOLDS,
        lan::GENA_ROUNDTRIP,
        lan::TOPOLOGY,
    ] {
        assert_eq!(
            status(&report, id),
            Status::Pass,
            "{id}: {:?}",
            report.get(id)
        );
    }
    assert_eq!(report.exit_code(), EXIT_WARN);
}

#[test]
fn dead_ssdp_without_seeds_fails_and_skips_what_depends_on_it() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let silent = UdpSocket::bind("127.0.0.1:0").unwrap();
    let report = doctor_on(lan_without_ssdp(&sim, &silent), Vec::new());
    assert_eq!(status(&report, lan::SSDP), Status::Fail);
    assert_eq!(status(&report, lan::SEEDS), Status::Skip);
    for id in [
        lan::PLAYERS,
        lan::HOUSEHOLDS,
        lan::GENA_ROUNDTRIP,
        lan::TOPOLOGY,
    ] {
        let r = report.get(id).unwrap();
        assert_eq!(r.status, Status::Skip, "{id}: {r:?}");
        assert!(r.summary.contains("lan.ssdp failed"), "{id}: {}", r.summary);
    }
    assert_eq!(status(&report, lan::STORE_OPEN), Status::Pass);
    assert_eq!(report.exit_code(), EXIT_FAIL);
}

/// A host on guest Wi-Fi: the router is the only neighbor it has reached.
fn only_the_router() -> Neighbors {
    let router = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
    Neighbors {
        gateway: Some(router),
        interface: Some("en1".into()),
        hosts: vec![router],
    }
}

#[test]
fn no_player_and_only_the_router_in_reach_warns_of_client_isolation() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let silent = UdpSocket::bind("127.0.0.1:0").unwrap();
    let report = doctor_with(lan_without_ssdp(&sim, &silent), Vec::new(), || {
        Some(only_the_router())
    });
    // It runs although lan.ssdp failed: that is when it explains most.
    assert_eq!(status(&report, lan::SSDP), Status::Fail);
    let isolation = report.get(lan::ISOLATION).expect("lan.isolation ran");
    assert_eq!(isolation.status, Status::Warn, "{isolation:?}");
    assert!(
        isolation
            .remedy
            .as_deref()
            .is_some_and(|r| r.contains("guest Wi-Fi")),
        "{isolation:?}"
    );
    // Players that answer settle it, whatever the table shows.
    let report = doctor_with(lan_for(&sim), Vec::new(), || Some(only_the_router()));
    assert_eq!(status(&report, lan::ISOLATION), Status::Pass);
}

#[test]
fn rooms_discovery_never_found_warn_in_the_topology() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let silent = UdpSocket::bind("127.0.0.1:0").unwrap();
    // One seed per household: the Office and the Bedroom are known only from
    // their household's topology.
    let seeds = vec![
        sim.player("Kitchen").unwrap().ip,
        sim.player("Living Room").unwrap().ip,
    ];
    let report = doctor_on(lan_without_ssdp(&sim, &silent), seeds);
    let topology = report.get(lan::TOPOLOGY).unwrap();
    assert_eq!(topology.status, Status::Warn, "{topology:?}");
    let summary = &topology.summary;
    assert!(
        summary.contains("Office") && summary.contains("Bedroom"),
        "{summary}"
    );
    assert!(
        !summary.contains("Bridge"),
        "a bridge plays nothing: {summary}"
    );
    assert!(topology.remedy.as_deref().unwrap().contains("seeds.toml"));
}

#[test]
fn a_pending_software_update_warns_without_naming_where_it_comes_from() {
    let sim = SimHousehold::standard().spawn().unwrap();
    // Sonos offers the S2 household (Living Room, Bedroom) a newer version
    // than its players run; the S1 household is offered nothing.
    sim.offer_update("Living Room", Some("97.1-80312")).unwrap();
    let report = doctor(&sim, Vec::new());
    let updates = report.get(lan::UPDATES).expect("lan.updates ran");
    assert_eq!(updates.status, Status::Warn, "{updates:?}");
    assert!(
        updates
            .summary
            .contains("Sonos offers 97.1-80312 to the S2 household")
            && updates.summary.contains("86.10-80260"),
        "{}",
        updates.summary
    );
    assert!(
        updates
            .remedy
            .as_deref()
            .unwrap_or("")
            .contains("never installs")
    );
    // The offer's update and manifest addresses never surface.
    let shown = serde_json::to_string(updates).unwrap();
    assert!(
        !shown.contains("update.invalid") && !shown.contains("http://"),
        "{shown}"
    );
    assert_eq!(report.exit_code(), EXIT_WARN);
    // The read is read-only: every topology subscription it made is gone.
    let log = sim.gena_log();
    let zgt = |subscribed: bool| {
        log.iter()
            .filter(|e| e.service == "ZoneGroupTopology")
            .filter(|e| {
                if subscribed {
                    matches!(e.event, GenaEvent::Subscribed { .. })
                } else {
                    matches!(e.event, GenaEvent::Unsubscribed { .. })
                }
            })
            .count()
    };
    assert!(zgt(true) > 0 && zgt(true) == zgt(false), "{log:?}");
}
