//! Core workflows through the real `fsonos` binary against the virtual
//! households, in direct mode: discover, zones, transport and volume
//! control, and grouping. Every result is confirmed from the sim's own
//! state, read through its LAN transport, not from the CLI's word.

mod e2e;

use e2e::{Run, Scenario};
use fsonos_proto::control::{
    get_bass, get_loudness, get_position_info, get_transport_info, get_volume,
};
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
            .map_or_default(|g| {
                let mut rooms: Vec<String> = g
                    .members
                    .iter()
                    .filter(|m| !m.invisible)
                    .map(|m| m.zone_name.clone())
                    .collect();
                rooms.sort();
                rooms
            })
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
        "status shows playing from queue position 2, unmuted",
        run.ok()
            && status["transport_state"] == "playing"
            && status["track"]["queue_position"] == 2
            && status["mute"] == false,
        &run.stdout,
    );

    check_search(&mut s, kitchen);

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
fn eq_reads_and_changes_a_rooms_tone_and_undo_puts_it_back() {
    let mut s = Scenario::start("eq");
    s.sim(SimHousehold::standard());
    let kitchen = s.ip("Kitchen");
    let tone = |s: &Scenario| {
        let lan = s.lan();
        (
            get_bass(&lan, kitchen).ok(),
            get_loudness(&lan, kitchen).ok(),
        )
    };

    let run = s.cli(
        "eq-set",
        &["eq", "Kitchen", "--bass", "-3", "--loudness", "off"],
    );
    s.check(
        "eq-set",
        "cli",
        "says what the tone is now",
        run.ok() && run.stdout.contains("bass -3, treble 0, loudness off"),
        format!("{}{}", run.stdout, run.stderr),
    );
    s.check(
        "eq-set",
        "sim",
        "Kitchen's bass is -3 and loudness off",
        tone(&s) == (Some(-3), Some(false)),
        format!("{:?}", tone(&s)),
    );

    let run = s.cli("eq-read", &["--json", "eq", "Kitchen"]);
    let eq = json(&run);
    s.check(
        "eq-read",
        "cli",
        "fsonos eq --json reads the tone back",
        run.ok() && eq["bass"] == -3 && eq["treble"] == 0 && eq["loudness"] == false,
        &run.stdout,
    );

    let run = s.cli("eq-undo", &["undo"]);
    s.check(
        "eq-undo",
        "sim",
        "undo puts Kitchen's tone back to flat with loudness on",
        run.ok() && tone(&s) == (Some(0), Some(true)),
        format!("{:?}: {}{}", tone(&s), run.stdout, run.stderr),
    );
    s.finish();
}

#[test]
fn play_next_and_queue_add_to_the_queue_and_undo_takes_them_off() {
    let mut s = Scenario::start("queue-add");
    s.sim(SimHousehold::standard());
    let queue = |s: &mut Scenario, step: &str| {
        let run = s.cli(step, &["--json", "queue", "Living Room"]);
        json(&run)["items"].as_array().map_or_default(|items| {
            items
                .iter()
                .map(|i| i["title"].as_str().unwrap_or("").to_string())
                .collect::<Vec<_>>()
        })
    };

    let run = s.cli(
        "queue-end",
        &[
            "play",
            "Living Room",
            "spotify:album:0SimQueueAlbum00000001",
            "--title",
            "Suite",
            "--queue",
        ],
    );
    s.check(
        "queue-end",
        "cli",
        "an album goes to the end of the queue, its tracks counted",
        run.ok() && run.stdout.contains("queued 3 track(s)"),
        format!("{}{}", run.stdout, run.stderr),
    );
    let run = s.cli(
        "queue-next",
        &[
            "play",
            "Living Room",
            "spotify:track:0SimQueueTrack00000009",
            "--title",
            "Encore",
            "--next",
        ],
    );
    s.check(
        "queue-next",
        "cli",
        "a track can be added next",
        run.ok() && run.stdout.contains("queued 1 track(s)"),
        format!("{}{}", run.stdout, run.stderr),
    );
    let titles = queue(&mut s, "queue-read");
    s.check(
        "queue-read",
        "cli",
        "the queue holds the album's three tracks and the track",
        titles.len() == 4 && titles.iter().any(|t| t == "Encore"),
        format!("{titles:?}"),
    );

    let run = s.cli("queue-undo", &["undo"]);
    let titles = queue(&mut s, "queue-after-undo");
    s.check(
        "queue-undo",
        "cli",
        "undo takes the newest addition off again",
        run.ok() && titles.len() == 3 && !titles.iter().any(|t| t == "Encore"),
        format!("{titles:?}: {}{}", run.stdout, run.stderr),
    );
    let run = s.cli(
        "queue-refused",
        &[
            "play",
            "Living Room",
            "x-rincon-mp3radio://example.invalid/a.mp3",
            "--queue",
        ],
    );
    s.check(
        "queue-refused",
        "cli",
        "a stream can't be queued: exit 2 (usage)",
        run.code == Some(2),
        format!("{:?}: {}", run.code, run.stderr),
    );
    s.finish();
}

/// A doctor report's checks that warn or fail, which a whole report
/// (truncated in a failure's detail) can hide.
fn flagged(report: &Value) -> String {
    report["checks"].as_array().map_or_default(|c| {
        c.iter()
            .filter(|x| x["status"] == "warn" || x["status"] == "fail")
            .map(|x| {
                format!(
                    "{} {}: {} ({})",
                    x["id"], x["status"], x["summary"], x["detail"]
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
    })
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

    // A host need not have a speech engine (the rch workers have none, CI
    // installs one): name one that is there. Doctor only checks it exists.
    let engine = std::env::current_exe().unwrap().display().to_string();
    std::fs::write(
        s.dir().join("data/speech.toml"),
        format!("backend = \"command\"\ncommand = [{engine:?}, \"{{output}}\"]\n"),
    )
    .unwrap();

    // Under the harness the HTTP address is 127.0.0.1:0: nothing to probe.
    let run = s.cli("doctor", &["doctor", "--json"]);
    let report = json(&run);
    // The harness sets no Spotify app: the DJ has nothing to play, which is
    // a warning (sp4f.13), and the only thing wrong here. The free space is
    // the host's, not the setup's (its thresholds are unit tests): a worker
    // short of disk may warn about it too.
    let gaps = report["checks"].as_array().map_or_default(|c| {
        c.iter()
            .filter(|x| x["status"] == "warn" || x["status"] == "fail")
            .map(|x| x["id"].as_str().unwrap_or_default().to_owned())
            .filter(|id| id != "store.disk")
            .collect::<Vec<_>>()
    });
    s.check(
        "doctor",
        "cli",
        "a setup whose one gap is Spotify warns about that alone: exit 6 with schema 1",
        run.code == Some(6)
            && report["schema"] == 1
            && report["exit_code"] == 6
            && gaps == ["dj.sign_in"],
        format!("exit {:?}: {} | {}", run.code, flagged(&report), run.stderr),
    );
    check_disk(&mut s, &report);
    s.check(
        "doctor",
        "cli",
        "the listener addresses pass; the daemon probe skips port 0",
        status(&report, "daemon.bind").as_deref() == Some("pass")
            && status(&report, "daemon.health").as_deref() == Some("skip"),
        &run.stdout,
    );
    let spotify: Vec<&Value> = report["checks"].as_array().map_or_default(|c| {
        c.iter()
            .filter(|x| x["id"].as_str().is_some_and(|i| i.starts_with("spotify")))
            .collect()
    });
    s.check(
        "doctor",
        "cli",
        "Spotify linkage checks run for both households and pass",
        !spotify.is_empty() && spotify.iter().all(|x| x["status"] == "pass"),
        &run.stdout,
    );

    check_mcp_doctor(&mut s);

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
        .map_or_default(|a| a.to_string());
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

/// An agent on stdio gets the same doctor checks as `fsonos doctor`, not only
/// Spotify's.
fn check_mcp_doctor(s: &mut Scenario) {
    let mut mcp = s.mcp();
    mcp.initialize();
    let call = mcp.request(
        "tools/call",
        &serde_json::json!({ "name": "doctor", "arguments": {} }),
    );
    let text = call["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    s.check(
        "doctor-mcp",
        "mcp",
        "fsonos mcp's doctor runs the store, LAN, alias and daemon checks too",
        [
            "store.open",
            "lan.players",
            "rooms.aliases",
            "daemon.bind",
            "daemon.health",
        ]
        .iter()
        .all(|id| text.contains(id)),
        &text,
    );
    drop(mcp);
}

/// Doctor's `store.disk` read the data directory's volume (vg3s.8).
fn check_disk(s: &mut Scenario, report: &Value) {
    let disk = report["checks"]
        .as_array()
        .and_then(|c| c.iter().find(|x| x["id"] == "store.disk"))
        .cloned()
        .unwrap_or_default();
    let evidence = &disk["evidence"];
    s.check(
        "doctor-disk",
        "cli",
        "doctor reads the free space on the data directory's volume",
        evidence["total_bytes"].as_u64().unwrap_or_default() > 0
            && evidence["free_bytes"].as_u64() <= evidence["total_bytes"].as_u64(),
        disk.to_string(),
    );
}

/// `fsonos play --search`: a bare --pick lists, a search plays its best
/// match, and one that finds nothing is NO_MATCH.
fn check_search(s: &mut Scenario, kitchen: std::net::IpAddr) {
    let run = s.cli(
        "search-pick",
        &["play", "Kitchen", "--search", "sim radio", "--pick"],
    );
    s.check(
        "search-pick",
        "cli",
        "--search with a bare --pick lists the matches without playing",
        run.ok() && run.stdout.starts_with("1. Sim Radio") && run.stdout.contains("[favorite]"),
        &run.stdout,
    );
    let run = s.cli("search-play", &["play", "Kitchen", "--search", "sim radio"]);
    let uri = get_position_info(&s.lan(), kitchen).map_or_default(|p| p.uri);
    s.check(
        "search-play",
        "cli",
        "--search plays the best match: the Sim Radio station",
        run.ok() && uri.starts_with("x-rincon-mp3radio:"),
        format!("{}; now on {uri}", run.stderr),
    );
    let run = s.cli("search-none", &["play", "Kitchen", "--search", "zzzqqq"]);
    s.check(
        "search-none",
        "cli",
        "a search that finds nothing exits 3 as NO_MATCH",
        run.code == Some(3) && run.stderr.contains("error[NO_MATCH]"),
        &run.stderr,
    );
    // A pick past the matches is NO_MATCH, as HTTP and MCP answer it.
    let run = s.cli(
        "search-pick-past",
        &["play", "Kitchen", "--search", "sim radio", "--pick", "9"],
    );
    s.check(
        "search-pick-past",
        "cli",
        "--pick past the matches exits 3 as NO_MATCH",
        run.code == Some(3) && run.stderr.contains("error[NO_MATCH]"),
        &run.stderr,
    );
}
