//! Core workflows through the real `fsonos` binary against the virtual
//! households, in direct mode: discover, zones, transport and volume
//! control, and grouping. Every result is confirmed from the sim's own
//! state, read through its LAN transport, not from the CLI's word.

mod e2e;

use e2e::{Run, Scenario};
use fsonos_proto::control::{get_position_info, get_transport_info, get_volume};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::SimHousehold;
use fsonos_types::TransportState;
use serde_json::Value;

fn json(run: &Run) -> Value {
    serde_json::from_str(&run.stdout).unwrap_or(Value::Null)
}

#[test]
fn discover_lists_every_renderer_with_its_generation() {
    let mut s = Scenario::start("discover");
    s.sim(SimHousehold::standard());
    let run = s.cli("discover", &["discover", "--json"]);
    s.check("discover", "cli", "exits 0", run.ok(), &run.stderr);
    let found = json(&run);
    let players = found["players"].as_array().cloned().unwrap_or_default();
    let generation = |room: &str| {
        players
            .iter()
            .find(|p| p["room"] == room)
            .map(|p| p["generation"].as_str().unwrap_or("").to_string())
    };
    s.check(
        "discover",
        "cli",
        "the four renderers are listed (the Bridge is not a room)",
        players.len() == 4,
        &found,
    );
    for (room, want) in [
        ("Kitchen", "S1"),
        ("Office", "S1"),
        ("Living Room", "S2"),
        ("Bedroom", "S2"),
    ] {
        s.check(
            "discover",
            "cli",
            &format!("{room} is {want}"),
            generation(room).as_deref() == Some(want),
            &found,
        );
    }
    s.finish();
}

#[test]
fn zones_match_the_sims_topology() {
    let mut s = Scenario::start("zones");
    s.sim(SimHousehold::standard());
    let run = s.cli("zones", &["zones", "--json"]);
    s.check("zones", "cli", "exits 0", run.ok(), &run.stderr);
    let zones = json(&run).as_array().cloned().unwrap_or_default();
    let groups =
        |room: &str| get_zone_group_state(&s.lan(), s.ip(room)).map_or(0, |t| t.groups.len());
    let topology = groups("Kitchen") + groups("Bedroom");
    s.check(
        "zones",
        "cli",
        "one zone per standalone room in both households",
        zones.len() == 4,
        format!("{} zones; sim lists {topology} groups", zones.len()),
    );
    let households: Vec<&str> = zones
        .iter()
        .filter_map(|z| z["household"].as_str())
        .collect();
    s.check(
        "zones",
        "cli",
        "zones carry their household label",
        households.contains(&"S1") && households.contains(&"S2"),
        households.join(","),
    );
    s.finish();
}

#[test]
fn control_reaches_the_coordinator_and_the_sim_agrees() {
    let mut s = Scenario::start("control");
    s.sim(SimHousehold::standard());
    let kitchen = s.ip("Kitchen");
    let state = |s: &Scenario| get_transport_info(&s.lan(), kitchen).map(|t| t.state).ok();
    let volume = |s: &Scenario| get_volume(&s.lan(), kitchen).ok();

    let stream = "x-rincon-mp3radio://stream.example.org/live.mp3";
    let run = s.cli("play", &["play", "Kitchen", stream]);
    s.check("play", "cli", "exits 0", run.ok(), &run.stderr);
    s.check(
        "play",
        "sim",
        "Kitchen is playing",
        state(&s) == Some(TransportState::Playing),
        format!("{:?}", state(&s)),
    );

    let run = s.cli("pause", &["pause", "kitchen"]);
    s.check("pause", "cli", "exits 0", run.ok(), &run.stderr);
    s.check(
        "pause",
        "sim",
        "Kitchen is paused",
        state(&s) == Some(TransportState::Paused),
        format!("{:?}", state(&s)),
    );

    let run = s.cli("resume", &["resume", "Kitchen"]);
    s.check("resume", "cli", "exits 0", run.ok(), &run.stderr);
    s.check(
        "resume",
        "sim",
        "Kitchen is playing again",
        state(&s) == Some(TransportState::Playing),
        format!("{:?}", state(&s)),
    );

    let run = s.cli("volume-set", &["volume", "Kitchen", "30", "--json"]);
    s.check(
        "volume-set",
        "cli",
        "reports 30",
        run.ok() && json(&run)["volume"] == 30,
        &run.stdout,
    );
    s.check(
        "volume-set",
        "sim",
        "Kitchen is at 30",
        volume(&s) == Some(30),
        format!("{:?}", volume(&s)),
    );
    let run = s.cli("volume-up", &["volume", "Kitchen", "+5"]);
    s.check("volume-up", "cli", "exits 0", run.ok(), &run.stderr);
    s.check(
        "volume-up",
        "sim",
        "Kitchen is at 35",
        volume(&s) == Some(35),
        format!("{:?}", volume(&s)),
    );
    let run = s.cli("volume-down", &["volume", "Kitchen", "-10"]);
    s.check("volume-down", "cli", "exits 0", run.ok(), &run.stderr);
    s.check(
        "volume-down",
        "sim",
        "Kitchen is at 25",
        volume(&s) == Some(25),
        format!("{:?}", volume(&s)),
    );

    // A single stream has no next track: the speaker's own fault comes back coded.
    let run = s.cli("next-on-stream", &["next", "Kitchen"]);
    s.check(
        "next-on-stream",
        "cli",
        "next on a stream is UPNP_FAULT 701 (exit 1)",
        run.code == Some(1)
            && run.stderr.contains("error[UPNP_FAULT]")
            && run.stderr.contains("701"),
        &run.stderr,
    );

    let run = s.cli("unknown-room", &["pause", "Kitchn"]);
    s.check(
        "unknown-room",
        "cli",
        "unknown room exits 3 with a suggestion",
        run.code == Some(3) && run.stderr.contains("did you mean: Kitchen@S1"),
        &run.stderr,
    );
    s.finish();
}

#[test]
fn grouping_joins_and_leaves_within_a_household() {
    let mut s = Scenario::start("grouping");
    s.sim(SimHousehold::standard());
    let group_of = |s: &Scenario, room: &str| {
        get_zone_group_state(&s.lan(), s.ip("Kitchen"))
            .ok()
            .and_then(|t| {
                t.groups
                    .into_iter()
                    .find(|g| g.members.iter().any(|m| m.zone_name == room))
            })
            .map(|g| {
                let mut rooms: Vec<String> = g
                    .members
                    .iter()
                    .filter(|m| !m.invisible)
                    .map(|m| m.zone_name.clone())
                    .collect();
                rooms.sort();
                rooms
            })
            .unwrap_or_default()
    };

    let run = s.cli("group", &["group", "Office", "Kitchen"]);
    s.check("group", "cli", "exits 0", run.ok(), &run.stderr);
    s.check(
        "group",
        "sim",
        "Kitchen and Office share a group",
        group_of(&s, "Office") == ["Kitchen", "Office"],
        format!("{:?}", group_of(&s, "Office")),
    );
    let run = s.cli("zones-grouped", &["zones", "--json"]);
    let zones = json(&run).as_array().cloned().unwrap_or_default();
    s.check(
        "zones-grouped",
        "cli",
        "zones shows the new group",
        zones
            .iter()
            .any(|z| z["members"].as_array().is_some_and(|m| m.len() == 2)),
        &run.stdout,
    );

    let run = s.cli("ungroup", &["ungroup", "Office"]);
    s.check("ungroup", "cli", "exits 0", run.ok(), &run.stderr);
    s.check(
        "ungroup",
        "sim",
        "Office plays on its own again",
        group_of(&s, "Office") == ["Office"],
        format!("{:?}", group_of(&s, "Office")),
    );

    let run = s.cli("cross-household", &["group", "Bedroom", "Kitchen"]);
    s.check(
        "cross-household",
        "cli",
        "S1 and S2 rooms cannot be grouped (CROSS_HOUSEHOLD_GROUP, exit 2)",
        run.code == Some(2) && run.stderr.contains("error[CROSS_HOUSEHOLD_GROUP]"),
        &run.stderr,
    );
    s.finish();
}

#[test]
fn favorites_play_and_the_queue_moves() {
    let mut s = Scenario::start("favorites");
    s.sim(SimHousehold::standard());
    let kitchen = s.ip("Kitchen");

    let run = s.cli("favorites", &["favorites", "Kitchen", "--json"]);
    let favorites = json(&run).as_array().cloned().unwrap_or_default();
    let titles: Vec<&str> = favorites
        .iter()
        .filter_map(|f| f["title"].as_str())
        .collect();
    s.check(
        "favorites",
        "cli",
        "the household's favorites are listed with their kinds",
        run.ok()
            && titles.contains(&"Sim Symphonies")
            && titles.contains(&"Sim Radio")
            && favorites.iter().any(|f| f["kind"] == "container"),
        &run.stdout,
    );

    let run = s.cli(
        "play-favorite",
        &["play", "Kitchen", "--favorite", "symphonies"],
    );
    s.check("play-favorite", "cli", "exits 0", run.ok(), &run.stderr);
    let state = get_transport_info(&s.lan(), kitchen).map(|t| t.state).ok();
    let track = |s: &Scenario| get_position_info(&s.lan(), kitchen).map(|p| p.track).ok();
    s.check(
        "play-favorite",
        "sim",
        "the album replaced the queue and plays from track 1",
        state == Some(TransportState::Playing) && track(&s) == Some(1),
        format!("{state:?}, track {:?}", track(&s)),
    );

    let run = s.cli("next-in-queue", &["next", "Kitchen"]);
    s.check("next-in-queue", "cli", "exits 0", run.ok(), &run.stderr);
    s.check(
        "next-in-queue",
        "sim",
        "the queue moved to track 2",
        track(&s) == Some(2),
        format!("track {:?}", track(&s)),
    );

    let run = s.cli("status", &["status", "Kitchen", "--json"]);
    let status = json(&run);
    s.check(
        "status",
        "cli",
        "status shows playing from queue position 2",
        run.ok()
            && status["transport_state"] == "playing"
            && status["track"]["queue_position"] == 2,
        &run.stdout,
    );

    let run = s.cli(
        "unknown-favorite",
        &["play", "Kitchen", "--favorite", "Symphonees"],
    );
    s.check(
        "unknown-favorite",
        "cli",
        "an unknown favorite exits 3 as UNKNOWN_FAVORITE with a hint",
        run.code == Some(3)
            && run.stderr.contains("error[UNKNOWN_FAVORITE]")
            && run.stderr.contains("list_favorites"),
        &run.stderr,
    );
    s.finish();
}

#[test]
fn the_log_records_actions_and_undo_restores_them() {
    let mut s = Scenario::start("undo");
    s.sim(SimHousehold::standard());
    let kitchen = s.ip("Kitchen");
    let volume = |s: &Scenario| get_volume(&s.lan(), kitchen).ok();
    let before = volume(&s);

    let run = s.cli("volume", &["volume", "Kitchen", "45"]);
    s.check("volume", "cli", "exits 0", run.ok(), &run.stderr);
    s.check(
        "volume",
        "sim",
        "Kitchen is at 45",
        volume(&s) == Some(45),
        format!("{:?}", volume(&s)),
    );

    let run = s.cli("log", &["log", "--json"]);
    let actions = json(&run).as_array().cloned().unwrap_or_default();
    s.check(
        "log",
        "cli",
        "the log lists the CLI's volume change as undoable",
        run.ok()
            && actions.first().is_some_and(|a| {
                a["client"] == "cli"
                    && a["intent"]
                        .as_str()
                        .is_some_and(|i| i.starts_with("set_volume"))
                    && a["undoable"] == true
            }),
        &run.stdout,
    );

    let run = s.cli("undo", &["undo", "--json"]);
    s.check(
        "undo",
        "cli",
        "undo reports the action it reversed",
        run.ok() && json(&run)["undone"].is_i64(),
        format!("{}{}", run.stdout, run.stderr),
    );
    s.check(
        "undo",
        "sim",
        "Kitchen's volume is back where it was",
        volume(&s) == before,
        format!("before {before:?}, now {:?}", volume(&s)),
    );

    let run = s.cli("log-after-undo", &["log", "--json", "--limit", "1"]);
    let newest = json(&run)[0].clone();
    s.check(
        "log-after-undo",
        "cli",
        "the undo is logged and points at what it reversed",
        newest["undo_of"].is_i64(),
        &run.stdout,
    );
    s.finish();
}

#[test]
fn doctor_reports_the_setup_with_its_exit_codes() {
    let mut s = Scenario::start("doctor");
    s.sim(SimHousehold::standard());
    let status = |report: &Value, id: &str| {
        report["checks"]
            .as_array()
            .and_then(|c| c.iter().find(|x| x["id"] == id))
            .map(|x| x["status"].as_str().unwrap_or("").to_string())
    };

    // Under the harness the HTTP address is 127.0.0.1:0: nothing to probe.
    let run = s.cli("doctor", &["doctor", "--json"]);
    let report = json(&run);
    s.check(
        "doctor",
        "cli",
        "an all-pass setup exits 0 with schema 1",
        run.code == Some(0) && report["schema"] == 1 && report["exit_code"] == 0,
        format!("exit {:?}: {}", run.code, run.stdout),
    );
    s.check(
        "doctor",
        "cli",
        "the listener addresses pass; the daemon probe skips port 0",
        status(&report, "daemon.bind").as_deref() == Some("pass")
            && status(&report, "daemon.health").as_deref() == Some("skip"),
        &run.stdout,
    );
    let spotify: Vec<&Value> = report["checks"]
        .as_array()
        .map(|c| {
            c.iter()
                .filter(|x| x["id"].as_str().is_some_and(|i| i.starts_with("spotify")))
                .collect()
        })
        .unwrap_or_default();
    s.check(
        "doctor",
        "cli",
        "Spotify linkage checks run for both households and pass",
        !spotify.is_empty() && spotify.iter().all(|x| x["status"] == "pass"),
        &run.stdout,
    );

    let run = s.cli("doctor-only", &["doctor", "--json", "--only", "spotify"]);
    s.check(
        "doctor-only",
        "cli",
        "--only spotify keeps only those checks; all pass: exit 0",
        run.code == Some(0)
            && json(&run)["checks"].as_array().is_some_and(|c| {
                c.iter()
                    .all(|x| x["id"].as_str().is_some_and(|i| i.starts_with("spotify")))
            }),
        format!("exit {:?}: {}", run.code, run.stdout),
    );

    // A port nothing listens on: no daemon answers there.
    let free = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.to_string())
        .unwrap_or_default();
    let run = s.cli("doctor-no-daemon", &["doctor", "--json", "--http", &free]);
    s.check(
        "doctor-no-daemon",
        "cli",
        "no daemon answering is a warning: exit 6",
        run.code == Some(6) && status(&json(&run), "daemon.health").as_deref() == Some("warn"),
        format!("exit {:?}: {}", run.code, run.stdout),
    );

    let run = s.cli(
        "doctor-unsafe",
        &["doctor", "--json", "--http", "0.0.0.0:0"],
    );
    let report = json(&run);
    s.check(
        "doctor-unsafe",
        "cli",
        "an unsafe bind address fails: exit 7",
        run.code == Some(7) && status(&report, "daemon.bind").as_deref() == Some("fail"),
        format!("exit {:?}: {}", run.code, run.stdout),
    );
    s.finish();
}
