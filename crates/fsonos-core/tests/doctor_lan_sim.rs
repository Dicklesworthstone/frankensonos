//! The doctor's LAN checks against `fsonos-sim` through the real LAN
//! transport (routed to the virtual players' loopback sockets), healthy and
//! with faults: swallowed NOTIFYs and a player that went offline.

use fsonos_core::doctor::lan::{self, LanProbe};
use fsonos_core::doctor::{EXIT_FAIL, EXIT_OK, Report, Runner, Status};
use fsonos_proto::net::Lan;
use fsonos_sim::{NotifyDrop, SimHandle, SimHousehold};
use std::net::IpAddr;
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

fn doctor(sim: &SimHandle, seeds: Vec<IpAddr>) -> Report {
    let dir = tempfile::tempdir().unwrap();
    let probe = Arc::new(LanProbe::new(lan_for(sim), seeds).with_ssdp_wait(Duration::from_secs(1)));
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
