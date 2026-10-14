//! The Spotify doctor checks against `fsonos-sim` over real loopback
//! sockets: both virtual households pass with the parameters the simulator
//! accepts; a household whose Spotify favorites are gone fails with the
//! remedy and its render-parameter check is skipped; a generation that is
//! not on the network is skipped.

use fsonos_core::HouseholdState;
use fsonos_core::doctor::{CheckId, EXIT_FAIL, EXIT_OK, Runner, Status, spotify};
use fsonos_proto::didl::spotify_uri_from_renderer_uri;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use std::sync::Arc;

fn both() -> SimHandle {
    SimHousehold::builder()
        .s1([SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1)])
        .s2([SimPlayerSpec::new("Living Room", SimModel::One)])
        .spawn()
        .unwrap()
}

fn run(sim: &SimHandle, rooms: &[&str]) -> fsonos_core::doctor::Report {
    let lan = Arc::new(sim.lan());
    let houses: Vec<HouseholdState> = rooms
        .iter()
        .map(|room| {
            let mut st = HouseholdState::default();
            st.apply_topology(&get_zone_group_state(&*lan, sim.player(room).unwrap().ip).unwrap());
            st
        })
        .collect();
    let mut runner = Runner::new();
    spotify::register(&mut runner, &lan, &houses);
    runner.run().unwrap()
}

fn status(report: &fsonos_core::doctor::Report, id: &'static str) -> Status {
    report.get(CheckId(id)).unwrap().status
}

#[test]
fn both_generations_pass_with_the_parameters_the_players_accept() {
    let sim = both();
    let report = run(&sim, &["Kitchen", "Living Room"]);
    assert_eq!(report.entries.len(), 6);
    for e in &report.entries {
        assert_eq!(e.result.status, Status::Pass, "{}: {:?}", e.id, e.result);
    }
    for (id, sw_gen) in [
        ("spotify.render_params.s1", 1),
        ("spotify.render_params.s2", 2),
    ] {
        let expected = sim.render_params(sw_gen).unwrap();
        let evidence = &report.get(CheckId(id)).unwrap().evidence;
        assert_eq!(evidence["flags"], expected.flags, "{id}");
        assert_eq!(evidence["sn"], expected.sn, "{id}");
        assert_eq!(evidence["item_id_prefix"], expected.item_id_prefix, "{id}");
        assert_eq!(evidence["rebuilt"], evidence["tracks"], "{id}");
    }
    let json = report.to_json().to_string() + &report.render_table();
    assert!(
        !json.contains(&sim.render_params(1).unwrap().cdudn),
        "the service-account descriptor is never shown in full"
    );
    let browses = sim
        .soap_log()
        .iter()
        .filter(|e| e.action == "Browse")
        .count();
    assert_eq!(browses, 2, "one Browse FV:2 per household");
    assert_eq!(report.exit_code(), EXIT_OK);
}

#[test]
fn a_household_without_spotify_favorites_fails_with_the_remedy() {
    let sim = both();
    sim.retain_favorites(1, |uri| spotify_uri_from_renderer_uri(uri).is_none())
        .unwrap();
    let report = run(&sim, &["Kitchen", "Living Room"]);

    let favorite = report.get(CheckId("spotify.favorite.s1")).unwrap();
    assert_eq!(favorite.status, Status::Fail, "{favorite:?}");
    let remedy = favorite.remedy.as_deref().unwrap();
    assert!(
        remedy.contains("S1 Controller app")
            && remedy.contains("add any Spotify track to My Sonos"),
        "{remedy}"
    );
    let linked = report.get(CheckId("spotify.linked.s1")).unwrap();
    assert_eq!(linked.status, Status::Warn, "linkage unknown: {linked:?}");
    assert!(
        linked
            .remedy
            .as_deref()
            .unwrap()
            .contains("link your Spotify account")
    );
    let params = report.get(CheckId("spotify.render_params.s1")).unwrap();
    assert_eq!(params.status, Status::Skip);
    assert!(
        params.summary.contains("spotify.favorite.s1 failed"),
        "{}",
        params.summary
    );

    for id in [
        "spotify.favorite.s2",
        "spotify.linked.s2",
        "spotify.render_params.s2",
    ] {
        assert_eq!(
            status(&report, id),
            Status::Pass,
            "{id}: the S2 household is unaffected"
        );
    }
    assert_eq!(report.exit_code(), EXIT_FAIL);
}

#[test]
fn a_generation_that_is_not_on_the_network_is_skipped() {
    let sim = SimHousehold::builder()
        .s2([SimPlayerSpec::new("Living Room", SimModel::One)])
        .spawn()
        .unwrap();
    let report = run(&sim, &["Living Room"]);
    for id in [
        "spotify.favorite.s1",
        "spotify.linked.s1",
        "spotify.render_params.s1",
    ] {
        assert_eq!(status(&report, id), Status::Skip, "{id}");
    }
    assert_eq!(status(&report, "spotify.render_params.s2"), Status::Pass);
    assert_eq!(report.exit_code(), EXIT_OK);
}
