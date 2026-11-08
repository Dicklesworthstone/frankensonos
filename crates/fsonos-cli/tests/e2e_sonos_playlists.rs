//! Discover and use native saved Sonos playlists directly and through the
//! daemon's real HTTP API; no Spotify token or My Sonos favorite is needed.

mod e2e;

use e2e::{Run, Scenario};
use fsonos_api::surface::queue::QueueAddedDto;
use fsonos_api::surface::sonos_playlists::SonosPlaylistDto;
use fsonos_proto::content::{QUEUE, browse_all};
use fsonos_proto::control as soap;
use fsonos_sim::SimHousehold;
use fsonos_types::TransportState;
use std::time::Duration;

fn setup(name: &str) -> Scenario {
    let mut s = Scenario::start(name);
    let sim = s.sim(SimHousehold::standard());
    sim.retain_favorites(1, |_| false).unwrap();
    sim.retain_favorites(2, |_| false).unwrap();
    s
}

fn call(s: &mut Scenario, route: &str, step: &str, args: &[&str]) -> Run {
    let missing = s.dir().join("no-client-seeds.toml");
    let mut command = vec![route, "--json"];
    if route == "--daemon" {
        // A local fallback cannot read this file: every daemon call must use
        // the daemon's warm catalog and control state.
        command.extend(["--seeds", missing.to_str().unwrap()]);
    }
    command.extend_from_slice(args);
    s.cli(step, &command)
}

fn exercise(s: &mut Scenario, route: &str) {
    for (room, generation) in [("Kitchen", "S1"), ("Living Room", "S2")] {
        let host = s.ip(room);
        let catalog = call(s, route, "native-list", &["playlists", room]);
        assert!(catalog.ok(), "{catalog:?}");
        let listed: Vec<SonosPlaylistDto> = serde_json::from_str(&catalog.stdout).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, "SQ:1");
        assert_eq!(listed[0].title, format!("Saved Mix {generation}"));
        let before = soap::get_transport_info(&s.lan(), host).unwrap().state;
        let queued = call(
            s,
            route,
            "native-append",
            &["play", room, "--playlist", "SQ:7", "--queue"],
        );
        assert!(queued.ok(), "{queued:?}");
        let added: QueueAddedDto = serde_json::from_str(&queued.stdout).unwrap();
        s.check(
            "native-append",
            "cli",
            "native append leaves playback unchanged",
            added.count == 3
                && added.first == 1
                && soap::get_transport_info(&s.lan(), host).unwrap().state == before,
            &queued.stdout,
        );

        let played = call(
            s,
            route,
            "native-play",
            &["play", room, "--playlist", &listed[0].title],
        );
        assert!(played.ok(), "{played:?}");
        let selected = browse_all(&s.lan(), host, QUEUE).unwrap();
        let playing = soap::get_position_info(&s.lan(), host).unwrap().uri;
        s.check(
            "native-play",
            "cli",
            "playing a named Sonos playlist replaces the queue",
            selected.len() == 2
                && played.stdout.contains("Sonos playlist")
                && soap::get_transport_info(&s.lan(), host).unwrap().state
                    == TransportState::Playing,
            &played.stdout,
        );

        let next = call(
            s,
            route,
            "native-next",
            &["play", room, "--playlist", "SQ:7", "--next"],
        );
        assert!(next.ok(), "{next:?}");
        let added: QueueAddedDto = serde_json::from_str(&next.stdout).unwrap();
        s.check(
            "native-next",
            "cli",
            "native next adds the saved order without restarting playback",
            added.first == 2
                && added.count == 3
                && added.playing_queue
                && browse_all(&s.lan(), host, QUEUE).unwrap().len() == 5
                && soap::get_position_info(&s.lan(), host).unwrap().uri == playing,
            &next.stdout,
        );
        let undone = call(s, route, "native-undo", &["undo"]);
        s.check(
            "native-undo",
            "cli",
            "undo removes only the confirmed native insertion",
            undone.ok() && browse_all(&s.lan(), host, QUEUE).unwrap() == selected,
            format!("{} {}", undone.stdout, undone.stderr),
        );
    }
}

#[test]
fn native_playlists_work_directly_on_both_sonos_generations() {
    let mut s = setup("sonos-playlists-direct");
    exercise(&mut s, "--direct");
    s.finish();
}

#[test]
fn native_playlists_use_the_daemons_http_catalog_control_and_action_log() {
    let mut s = setup("sonos-playlists-daemon");
    let mut daemon = s.spawn("serve", &["serve", "--http", "127.0.0.1:0"]);
    assert!(
        daemon
            .wait_line("fsonos serve: live", Duration::from_secs(20))
            .is_some()
    );
    exercise(&mut s, "--daemon");
    let log = call(&mut s, "--daemon", "native-log", &["log", "--limit", "20"]);
    s.check(
        "native-log",
        "cli",
        "the daemon records native playlist commands",
        log.ok() && log.stdout.contains("Sonos playlist"),
        format!("{} {}", log.stdout, log.stderr),
    );
    drop(daemon);
    s.finish();
}
