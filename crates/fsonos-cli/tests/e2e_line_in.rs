//! Native inputs through the actual CLI, both directly and through the daemon.
//! The simulator supplies the inputs; no cable, Spotify or physical LAN is used.

mod e2e;

use e2e::{Run, Scenario};
use fsonos_api::{LineInSourcesDto, OutcomeDto};
use fsonos_proto::content::{QUEUE, browse_all};
use fsonos_proto::control as soap;
use fsonos_sim::SimHousehold;
use fsonos_types::TransportState;
use std::time::Duration;

const RADIO: &str = "x-rincon-mp3radio://stream.example.org/prior";

fn setup(name: &str) -> Scenario {
    let mut scenario = Scenario::start(name);
    let sim = scenario.sim(SimHousehold::standard());
    let kitchen = sim.player("Kitchen").unwrap();
    let office = sim.player("Office").unwrap();
    let lan = sim.lan();
    // Office supplies the input, while Kitchen leads the destination group.
    soap::set_av_transport_uri(&lan, office.ip, &format!("x-rincon:{}", kitchen.uuid), "").unwrap();
    soap::add_uri_to_queue(
        &lan,
        kitchen.ip,
        "http://media.example.org/retained.mp3",
        "",
        false,
    )
    .unwrap();
    soap::set_av_transport_uri(&lan, kitchen.ip, RADIO, "").unwrap();
    soap::play(&lan, kitchen.ip).unwrap();
    scenario
}

fn call(scenario: &mut Scenario, route: &str, step: &str, args: &[&str]) -> Run {
    let missing = scenario.dir().join("no-client-seeds.toml");
    let mut command = vec![route, "--json"];
    if route == "--daemon" {
        // A local fallback cannot discover the house from this nonexistent file.
        command.extend(["--seeds", missing.to_str().unwrap()]);
    }
    command.extend_from_slice(args);
    scenario.cli(step, &command)
}

fn exercise(scenario: &mut Scenario, route: &str) {
    let host = scenario.ip("Kitchen");
    let input_host = scenario.ip("Office");
    let source = scenario
        .sim_handle()
        .unwrap()
        .player("Office")
        .unwrap()
        .uuid
        .clone();
    let queue = browse_all(&scenario.lan(), host, QUEUE).unwrap();
    let input_before = soap::get_media_info(&scenario.lan(), input_host)
        .unwrap()
        .uri;

    let listed = call(scenario, route, "line-in-list", &["line-in", "Office"]);
    assert!(listed.ok(), "{listed:?}");
    let sources: LineInSourcesDto = serde_json::from_str(&listed.stdout).unwrap();
    scenario.check(
        "line-in-list",
        "cli",
        "native input discovery preserves the exact grouped source without starting it",
        sources.sources.iter().any(|input| {
            input.player == source && input.room == "Office" && input.name == "Office line-in"
        }) && sources.unavailable.is_empty()
            && soap::get_media_info(&scenario.lan(), host).unwrap().uri == RADIO,
        &listed.stdout,
    );

    let played = call(
        scenario,
        route,
        "line-in-play",
        &["play", "Office", "--line-in", &source],
    );
    assert!(played.ok(), "{played:?}");
    let outcome: OutcomeDto = serde_json::from_str(&played.stdout).unwrap();
    scenario.check(
        "line-in-play",
        "cli",
        "the group's coordinator renders the exact input and keeps its queue and membership",
        outcome.changed
            && soap::get_media_info(&scenario.lan(), host).unwrap().uri
                == format!("x-rincon-stream:{source}")
            && soap::get_transport_info(&scenario.lan(), host)
                .unwrap()
                .state
                == TransportState::Playing
            && soap::get_media_info(&scenario.lan(), input_host)
                .unwrap()
                .uri
                == input_before
            && browse_all(&scenario.lan(), host, QUEUE).unwrap() == queue,
        &played.stdout,
    );

    let refused = call(
        scenario,
        route,
        "line-in-cross-household",
        &["play", "Living Room", "--line-in", &source],
    );
    scenario.check(
        "line-in-cross-household",
        "cli",
        "a native input cannot be redirected to another household",
        !refused.ok()
            && soap::get_media_info(&scenario.lan(), host).unwrap().uri
                == format!("x-rincon-stream:{source}"),
        format!("{} {}", refused.stdout, refused.stderr),
    );

    let restored = call(scenario, route, "line-in-undo", &["undo"]);
    scenario.check(
        "line-in-undo",
        "cli",
        "playback undo returns to the prior source without replacing the queue",
        restored.ok()
            && soap::get_media_info(&scenario.lan(), host).unwrap().uri == RADIO
            && soap::get_transport_info(&scenario.lan(), host)
                .unwrap()
                .state
                == TransportState::Playing
            && browse_all(&scenario.lan(), host, QUEUE).unwrap() == queue,
        format!("{} {}", restored.stdout, restored.stderr),
    );
}

#[test]
fn native_input_playback_works_through_the_direct_cli() {
    let mut scenario = setup("line-in-direct");
    exercise(&mut scenario, "--direct");
    scenario.finish();
}

#[test]
fn native_input_playback_uses_the_daemons_inventory_control_and_undo() {
    let mut scenario = setup("line-in-daemon");
    let mut daemon = scenario.spawn("serve", &["serve", "--http", "127.0.0.1:0"]);
    assert!(
        daemon
            .wait_line("fsonos serve: live", Duration::from_secs(20))
            .is_some()
    );
    exercise(&mut scenario, "--daemon");
    drop(daemon);
    scenario.finish();
}

#[test]
fn line_in_room_sets_reach_the_selected_groups_through_direct_and_daemon_cli() {
    for route in ["--direct", "--daemon"] {
        let mut scenario = Scenario::start(&format!("line-in-sets{route}"));
        scenario.sim(SimHousehold::standard());
        std::fs::write(
            scenario.dir().join("data/aliases.toml"),
            "[aliases]\nlistening = ['Kitchen', 'Office']\n",
        )
        .unwrap();
        let source = scenario
            .sim_handle()
            .unwrap()
            .player("Office")
            .unwrap()
            .uuid
            .clone();
        for room in ["Kitchen", "Office"] {
            let host = scenario.ip(room);
            soap::add_uri_to_queue(
                &scenario.lan(),
                host,
                "http://media.example.org/retained.mp3",
                "",
                false,
            )
            .unwrap();
            soap::set_av_transport_uri(&scenario.lan(), host, RADIO, "").unwrap();
            soap::play(&scenario.lan(), host).unwrap();
        }
        let daemon = if route == "--daemon" {
            let mut daemon = scenario.spawn("serve", &["serve", "--http", "127.0.0.1:0"]);
            assert!(
                daemon
                    .wait_line("fsonos serve: live", Duration::from_secs(20))
                    .is_some()
            );
            Some(daemon)
        } else {
            None
        };
        let before = scenario.sim_handle().unwrap().soap_log().len();
        let played = call(
            &mut scenario,
            route,
            "line-in-set-play",
            &["play", "listening", "--line-in", &source],
        );
        assert!(played.ok(), "{played:?}");
        let outcome: OutcomeDto = serde_json::from_str(&played.stdout).unwrap();
        assert!(outcome.complete());
        assert_eq!(outcome.batch.unwrap().results.len(), 2);
        let mut started: Vec<_> = scenario.sim_handle().unwrap().soap_log()[before..]
            .iter()
            .filter(|entry| entry.action == "Play")
            .map(|entry| entry.room.clone())
            .collect();
        started.sort();
        assert_eq!(started, ["Kitchen", "Office"]);
        for room in ["Kitchen", "Office"] {
            let host = scenario.ip(room);
            assert_eq!(
                soap::get_media_info(&scenario.lan(), host).unwrap().uri,
                format!("x-rincon-stream:{source}")
            );
            assert_eq!(browse_all(&scenario.lan(), host, QUEUE).unwrap().len(), 1);
        }
        let restored = call(&mut scenario, route, "line-in-set-undo", &["undo"]);
        assert!(restored.ok(), "{restored:?}");
        for room in ["Kitchen", "Office"] {
            assert_eq!(
                soap::get_media_info(&scenario.lan(), scenario.ip(room))
                    .unwrap()
                    .uri,
                RADIO
            );
        }

        let before = scenario.sim_handle().unwrap().soap_log().len();
        let refused = call(
            &mut scenario,
            route,
            "line-in-all-households",
            &["play", "all", "--line-in", &source],
        );
        assert!(!refused.ok(), "{refused:?}");
        assert!(
            scenario.sim_handle().unwrap().soap_log()[before..]
                .iter()
                .all(|entry| entry.action.starts_with("Get") || entry.action == "Browse")
        );

        scenario
            .sim_handle()
            .unwrap()
            .upnp_fault("Office", "SetAVTransportURI", 701)
            .unwrap();
        let before = scenario.sim_handle().unwrap().soap_log().len();
        let partial = call(
            &mut scenario,
            route,
            "line-in-partial",
            &["play", "all@S1", "--line-in", &source],
        );
        assert!(!partial.ok(), "{partial:?}");
        assert!(partial.stderr.contains("incomplete"), "{partial:?}");
        let outcome: OutcomeDto = serde_json::from_str(&partial.stdout).unwrap();
        assert_eq!(outcome.incomplete_targets(), 1);
        assert_eq!(outcome.batch.unwrap().results.len(), 2);
        assert_eq!(
            scenario.sim_handle().unwrap().soap_log()[before..]
                .iter()
                .filter(|entry| entry.room == "Office" && entry.action == "SetAVTransportURI")
                .count(),
            1
        );
        for room in ["Living Room", "Bedroom"] {
            assert_eq!(
                soap::get_transport_info(&scenario.lan(), scenario.ip(room))
                    .unwrap()
                    .state,
                TransportState::Stopped
            );
        }
        drop(daemon);
        scenario.finish();
    }
}
