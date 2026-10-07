//! The doctor's LAN checks against `fsonos-sim` through the real LAN
//! transport (routed to the virtual players' loopback sockets), healthy and
//! with faults: swallowed NOTIFYs and a player that went offline.

use fsonos_core::doctor::lan::{self, LanProbe};
use fsonos_core::doctor::{EXIT_FAIL, EXIT_OK, EXIT_WARN, Report, Runner, Status};
use fsonos_proto::net::Lan;
use fsonos_sim::{GenaEvent, NotifyDrop, SimHandle, SimHousehold};
use std::net::{IpAddr, UdpSocket};
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
    let dir = tempfile::tempdir().unwrap();
    let probe = Arc::new(LanProbe::new(lan, seeds).with_ssdp_wait(Duration::from_secs(1)));
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
    let count = |is: fn(&GenaEvent) -> bool| log.iter().filter(|e| is(&e.event)).count();
    assert_eq!(
        (
            count(|e| matches!(e, GenaEvent::Subscribed { .. })),
            count(|e| matches!(e, GenaEvent::Unsubscribed { .. })),
        ),
        (1, 1),
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
