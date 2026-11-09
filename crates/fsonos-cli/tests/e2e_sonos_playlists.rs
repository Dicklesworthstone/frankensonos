//! Discover and use native saved Sonos playlists directly and through the
//! daemon's real HTTP API; no Spotify token or My Sonos favorite is needed.

mod e2e;

use e2e::{Run, Scenario};
use fsonos_api::surface::queue::QueueAddedDto;
use fsonos_api::surface::sonos_playlists::{
    SavedSonosPlaylistDto, SonosPlaylistContentsDto, SonosPlaylistDto,
};
use fsonos_proto::content::{QUEUE, browse_all};
use fsonos_proto::control as soap;
use fsonos_proto::didl::DidlObject;
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

        inspect_saved_contents(s, route, room, &selected, &playing);

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

        save_and_recall(s, route, room, generation, &selected);
    }
}

fn inspect_saved_contents(
    s: &mut Scenario,
    route: &str,
    room: &str,
    selected: &[DidlObject],
    playing: &str,
) {
    let host = s.ip(room);
    let first = call(
        s,
        route,
        "native-contents",
        &["playlist-contents", room, "SQ:7", "--limit", "2"],
    );
    assert!(first.ok(), "{first:?}");
    let page: SonosPlaylistContentsDto = serde_json::from_str(&first.stdout).unwrap();
    assert_eq!(page.playlist.id, "SQ:7");
    assert_eq!(page.total, 3);
    assert_eq!(page.next_offset, Some(2));
    assert_eq!(page.items.len(), 2);
    let continuation = call(
        s,
        route,
        "native-contents-next",
        &[
            "playlist-contents",
            room,
            &page.playlist.id,
            "--limit",
            "2",
            "--offset",
            "2",
            "--expected-revision",
            &page.revision,
        ],
    );
    assert!(continuation.ok(), "{continuation:?}");
    let last: SonosPlaylistContentsDto = serde_json::from_str(&continuation.stdout).unwrap();
    s.check(
        "native-contents",
        "cli",
        "saved contents retain duplicates and leave current music untouched",
        last.revision == page.revision
            && last.items.len() == 1
            && last.items[0].position == 3
            && last.items[0].uri == page.items[0].uri
            && last.next_offset.is_none()
            && browse_all(&s.lan(), host, QUEUE).unwrap() == selected
            && soap::get_position_info(&s.lan(), host).unwrap().uri == playing
            && soap::get_transport_info(&s.lan(), host).unwrap().state == TransportState::Playing,
        format!("{} {}", first.stdout, continuation.stdout),
    );
    let stale = call(
        s,
        route,
        "native-contents-stale",
        &[
            "playlist-contents",
            room,
            &page.playlist.id,
            "--offset",
            "2",
            "--expected-revision",
            "stale",
        ],
    );
    s.check(
        "native-contents-stale",
        "cli",
        "stale saved playlist continuations are refused",
        !stale.ok()
            && (stale.stdout.contains("QUEUE_CHANGED") || stale.stderr.contains("QUEUE_CHANGED")),
        format!("{} {}", stale.stdout, stale.stderr),
    );
}

fn save_and_recall(
    s: &mut Scenario,
    route: &str,
    room: &str,
    generation: &str,
    selected: &[DidlObject],
) {
    let host = s.ip(room);
    let title = format!("Saved from CLI {generation}");
    let before_save = soap::get_position_info(&s.lan(), host).unwrap();
    let save = call(s, route, "native-save", &["playlist-save", room, &title]);
    assert!(save.ok(), "{save:?}");
    let saved: SavedSonosPlaylistDto = serde_json::from_str(&save.stdout).unwrap();
    s.check(
        "native-save",
        "cli",
        "saving confirms a new native playlist without interrupting the playing queue",
        saved.logged
            && saved.playlist.id == "SQ:8"
            && saved.playlist.title == title
            && browse_all(&s.lan(), host, QUEUE).unwrap() == selected
            && soap::get_position_info(&s.lan(), host).unwrap() == before_save,
        &save.stdout,
    );
    let catalog = call(s, route, "native-saved-catalog", &["playlists", room]);
    assert!(catalog.ok(), "{catalog:?}");
    let listed: Vec<SonosPlaylistDto> = serde_json::from_str(&catalog.stdout).unwrap();
    assert!(listed.contains(&saved.playlist));

    let replace = call(
        s,
        route,
        "native-replace",
        &["play", room, "--playlist", "SQ:7"],
    );
    assert!(replace.ok(), "{replace:?}");
    let recall = call(
        s,
        route,
        "native-recall-saved",
        &["play", room, "--playlist", &saved.playlist.id],
    );
    s.check(
        "native-recall-saved",
        "cli",
        "the newly saved playlist replays its original ordered queue",
        recall.ok() && browse_all(&s.lan(), host, QUEUE).unwrap() == selected,
        format!("{} {}", recall.stdout, recall.stderr),
    );
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
        log.ok()
            && log.stdout.contains("Sonos playlist")
            && log.stdout.contains("save_sonos_playlist"),
        format!("{} {}", log.stdout, log.stderr),
    );
    drop(daemon);
    s.finish();
}
