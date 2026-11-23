//! Curate saved playlists from explicit sources through both CLI routes.

mod e2e;

use e2e::{Run, Scenario};
use fsonos_api::surface::sonos_playlists::{
    AddedTracksToSonosPlaylistDto, SonosPlaylistContentsDto,
};
use fsonos_proto::content::{QUEUE, browse_all};
use fsonos_proto::control as soap;
use fsonos_sim::SimHousehold;
use std::time::Duration;

const TRACK: &str = "spotify:track:0000000000000000000001";
const EPISODE: &str = "spotify:episode:0000000000000000000002";

fn call(s: &mut Scenario, route: &str, step: &str, args: &[&str]) -> Run {
    let missing = s.dir().join("no-client-seeds.toml");
    let mut command = vec![route, "--json"];
    if route == "--daemon" {
        command.extend(["--seeds", missing.to_str().unwrap()]);
    }
    command.extend_from_slice(args);
    s.cli(step, &command)
}

fn contents(s: &mut Scenario, route: &str, room: &str) -> SonosPlaylistContentsDto {
    let result = call(
        s,
        route,
        "read-saved-destination",
        &["playlist-contents", room, "SQ:1"],
    );
    assert!(result.ok(), "{result:?}");
    serde_json::from_str(&result.stdout).unwrap()
}

fn exercise(s: &mut Scenario, route: &str) {
    for room in ["Kitchen", "Living Room"] {
        let host = s.ip(room);
        let queue = browse_all(&s.lan(), host, QUEUE).unwrap();
        let position = soap::get_position_info(&s.lan(), host).unwrap();
        let transport = soap::get_transport_info(&s.lan(), host).unwrap();
        let before = contents(s, route, room);
        let args = [
            "playlist-add-tracks",
            room,
            "SQ:1",
            TRACK,
            EPISODE,
            TRACK,
            "--expected-revision",
            &before.revision,
        ];
        let added = call(s, route, "add-explicit-saved-tracks", &args);
        assert!(added.ok(), "{added:?}");
        let receipt: AddedTracksToSonosPlaylistDto = serde_json::from_str(&added.stdout).unwrap();
        let after = contents(s, route, room);
        let count = before.items.len();
        s.check(
            "saved-explicit-order",
            "cli",
            "saved sources retain order and repeats on S1 and S2",
            receipt.playlist.id == "SQ:1"
                && receipt.added == 3
                && receipt.previous_total == before.total
                && receipt.total == before.total + 3
                && receipt.source_uris == [TRACK, EPISODE, TRACK]
                && receipt.revision == after.revision
                && receipt.logged
                && after.items[..count] == before.items
                && after.items[count].uri == after.items[count + 2].uri
                && after.items[count].uri != after.items[count + 1].uri,
            &added.stdout,
        );
        let stale = call(s, route, "reject-replayed-saved-addition", &args);
        assert!(!stale.ok(), "{stale:?}");
        assert_eq!(contents(s, route, room).items, after.items);
        s.check(
            "saved-explicit-playback",
            "cli",
            "saved curation does not mutate the live queue or playback",
            browse_all(&s.lan(), host, QUEUE).unwrap() == queue
                && soap::get_position_info(&s.lan(), host).unwrap() == position
                && soap::get_transport_info(&s.lan(), host).unwrap() == transport,
            &added.stdout,
        );
    }
}

#[test]
fn explicit_sources_append_to_saved_playlists_directly() {
    let mut s = Scenario::start("playlist-add-tracks-direct");
    s.sim(SimHousehold::standard());
    exercise(&mut s, "--direct");
    s.finish();
}

#[test]
fn explicit_sources_append_through_the_selected_daemon_without_local_fallback() {
    let mut s = Scenario::start("playlist-add-tracks-daemon");
    s.sim(SimHousehold::standard());
    let mut daemon = s.spawn("serve", &["serve", "--http", "127.0.0.1:0"]);
    assert!(
        daemon
            .wait_line("fsonos serve: live", Duration::from_secs(20))
            .is_some()
    );
    exercise(&mut s, "--daemon");
    let log = call(
        &mut s,
        "--daemon",
        "saved-addition-log",
        &["log", "--limit", "100"],
    );
    assert!(log.ok(), "{log:?}");
    assert!(log.stdout.contains("add_tracks_to_sonos_playlist"));
    drop(daemon);
    s.finish();
}
