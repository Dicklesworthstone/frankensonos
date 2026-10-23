//! Self-healing through `fsonos serve` against the simulator:
//! - a rebooted player's events resume, and a change made after the reboot
//!   shows in zone state within 5 s;
//! - a player another app takes out of its group becomes a coordinator, and
//!   its playback shows through its own events, with no survey in between;
//! - a command to a powered-off player fails cleanly as PLAYER_UNREACHABLE,
//!   with a hint.
//!
//! - a group-volume change made while the group's coordinator was re-elected
//!   behind the daemon's back follows the new coordinator (a HEALED note);
//! - a command to a player that moved to a new address (DHCP) behind the
//!   daemon's back finds it there (a HEALED note), and the next goes there
//!   at once.

mod e2e;

use e2e::{Scenario, http};
use fsonos_proto::control::{
    get_group_volume, get_volume, join_group, leave_group, play, set_av_transport_uri, set_volume,
};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{GenaEvent, NotifyDrop, SimHousehold};
use fsonos_types::PlayerId;
use serde_json::{Value, json};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

/// `key=value` out of a daemon line.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|word| word.strip_prefix(key)?.strip_prefix('='))
}

/// `GET path` as JSON (`Null` when it is not).
fn get(api: &str, path: &str) -> Value {
    http(api, "GET", path, &[], "")
        .ok()
        .and_then(|(_, _, body)| serde_json::from_str(&body).ok())
        .unwrap_or(Value::Null)
}

/// Poll `probe` every 100 ms until it holds or `within` has passed.
fn eventually(within: Duration, mut probe: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + within;
    loop {
        if probe() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The reads a live model's clock sample makes (fsonos-core live/observation).
const CLOCK_READS: [&str; 3] = ["GetTransportInfo", "GetMediaInfo", "GetPositionInfo"];

#[test]
fn serve_heals_around_reboots_regrouping_and_power_loss() {
    let mut s = Scenario::start("self_healing");
    s.sim(SimHousehold::standard());
    // Another app grouped the Office into the Kitchen before fsonos starts,
    // so the daemon's first survey sees the Office as a member only.
    let kitchen_uuid = s
        .sim_handle()
        .and_then(|sim| sim.player("Kitchen").map(|p| p.uuid.clone()))
        .unwrap_or_default();
    let joined = join_group(&s.lan(), s.ip("Office"), &PlayerId(kitchen_uuid));
    s.check(
        "setup",
        "sim",
        "the Office starts grouped with the Kitchen",
        joined.is_ok(),
        format!("{joined:?}"),
    );
    // Where the Kitchen will move to: routed before serve reads the routes.
    let spare = s.spare_route();

    let mut daemon = s.spawn(
        "serve",
        &[
            "serve",
            "--http",
            "127.0.0.1:0",
            "--mcp-http",
            "127.0.0.1:0",
        ],
    );
    let ready = daemon
        .wait_line("fsonos serve: ready", Duration::from_secs(20))
        .unwrap_or_default();
    let api = field(&ready, "http")
        .and_then(|u| u.strip_prefix("http://"))
        .unwrap_or("")
        .to_string();
    let live = daemon.wait_line("fsonos serve: live", Duration::from_secs(20));
    s.check(
        "live",
        "daemon",
        "serve's live model has surveyed both households",
        live.as_deref()
            .is_some_and(|l| field(l, "households") == Some("2")),
        daemon.seen.join("\n"),
    );

    check_reboot(&mut s, &api);
    check_ungrouped(&mut s, &api);
    check_reelect(&mut s, &api);
    check_moved(&mut s, spare);
    check_power_loss(&mut s, &api);

    let code = daemon.interrupt(Duration::from_secs(10));
    s.check(
        "stop",
        "daemon",
        "SIGINT stops serve cleanly (exit 0)",
        code == Some(0),
        format!("exit {code:?}"),
    );
    s.finish();
}

/// The S1 player that does not carry its household's topology subscription
/// reboots (so that subscription reports it); a volume set on it directly
/// after the reboot shows in zone state within 5 s.
fn check_reboot(s: &mut Scenario, api: &str) {
    let room = s
        .sim_handle()
        .and_then(|sim| {
            let topology: Vec<String> = sim
                .gena_log()
                .iter()
                .filter(|e| {
                    e.service == "ZoneGroupTopology"
                        && matches!(e.event, GenaEvent::Subscribed { .. })
                })
                .map(|e| e.player.clone())
                .collect();
            ["Kitchen", "Living Room", "Bedroom"]
                .iter()
                .filter_map(|room| sim.player(room))
                .find(|p| !topology.contains(&p.uuid) && !p.room.contains(' '))
                .map(|p| p.room.clone())
        })
        .unwrap_or_default();
    let rebooted = s.sim_handle().map(|sim| sim.reboot(&room, Duration::ZERO));
    s.check(
        "reboot",
        "sim",
        "a player reboots (its subscriptions are gone)",
        matches!(rebooted, Some(Ok(()))),
        format!("{room}: {rebooted:?}"),
    );
    std::thread::sleep(Duration::from_millis(300));
    let before = get(api, &format!("/zones/{room}/state"))["volume"].as_u64();
    let level = if before == Some(41) { 42 } else { 41 };
    let set = set_volume(&s.lan(), s.ip(&room), level);
    let mut state = Value::Null;
    let seen = set.is_ok()
        && eventually(Duration::from_secs(5), || {
            state = get(api, &format!("/zones/{room}/state"));
            state["volume"] == json!(level)
        });
    s.check(
        "reboot",
        "http",
        "a change made after the reboot shows in zone state within 5 s",
        seen,
        &state,
    );
}

/// Another app takes the Office out of the Kitchen's group: it becomes a
/// coordinator, and a stream started on it directly shows as playing within
/// 3 s through its own events, with nothing read from the players.
fn check_ungrouped(s: &mut Scenario, api: &str) {
    let office = s.ip("Office");
    let own = s
        .sim_handle()
        .and_then(|sim| sim.player("Office"))
        .map_or_else(|| PlayerId(String::new()), |p| PlayerId(p.uuid.clone()));
    let left = leave_group(&s.lan(), office, &own);
    s.check(
        "ungrouped",
        "sim",
        "the Office leaves the Kitchen's group",
        left.is_ok(),
        format!("{left:?}"),
    );
    std::thread::sleep(Duration::from_millis(300));
    // Position is never evented (real players neither), so the live model
    // reads a clock right after a transport event. The Office refuses those
    // reads meanwhile: the playing state the API shows can then only have
    // come from the Office's new subscription.
    let refused = s.sim_handle().is_some_and(|sim| {
        CLOCK_READS
            .iter()
            .all(|action| sim.upnp_fault("Office", action, 501).is_ok())
    });
    let polled = s.sim_handle().map_or(0, |sim| sim.soap_log().len());
    let started = set_av_transport_uri(
        &s.lan(),
        office,
        "x-rincon-mp3radio://stream.example.invalid/sim.mp3",
        "",
    )
    .and_then(|()| play(&s.lan(), office));
    let mut state = Value::Null;
    let seen = started.is_ok()
        && eventually(Duration::from_secs(3), || {
            state = get(api, "/zones/Office/state");
            state["transport_state"] == "playing"
        });
    s.check(
        "ungrouped",
        "http",
        "the Office's own playback shows within 3 s",
        seen,
        format!("{started:?} {state}"),
    );
    let answered: Vec<String> = s.sim_handle().map_or_else(Vec::new, |sim| {
        sim.soap_log()[polled..]
            .iter()
            .filter(|e| e.room == "Office" && e.action.starts_with("Get") && e.result.is_ok())
            .map(|e| e.action.clone())
            .collect()
    });
    let cleared = s
        .sim_handle()
        .is_some_and(|sim| sim.clear_faults("Office").is_ok());
    s.check(
        "ungrouped",
        "sim",
        "it came from the Office's new subscription: no read of the Office was answered",
        refused && cleared && answered.is_empty(),
        format!("refused={refused} cleared={cleared} answered={answered:?}"),
    );
}

/// The Bedroom loses power: a command to it fails as PLAYER_UNREACHABLE,
/// with a hint, and does not hang.
fn check_power_loss(s: &mut Scenario, api: &str) {
    let off = s.sim_handle().map(|sim| sim.set_offline("Bedroom", true));
    s.check(
        "power-loss",
        "sim",
        "the Bedroom player powers off",
        matches!(off, Some(Ok(()))),
        format!("{off:?}"),
    );
    let started = Instant::now();
    let body = json!({ "zone": "Bedroom", "volume": 20 }).to_string();
    let failed = http(
        api,
        "POST",
        "/volume",
        &[("Content-Type", "application/json")],
        &body,
    );
    let took = started.elapsed();
    let error: Value = failed
        .as_ref()
        .ok()
        .and_then(|(_, _, b)| serde_json::from_str(b).ok())
        .unwrap_or(Value::Null);
    s.check(
        "power-loss",
        "http",
        "the command fails as PLAYER_UNREACHABLE with a hint, within 15 s",
        failed.as_ref().is_ok_and(|(code, _, _)| *code == 503)
            && error.to_string().contains("PLAYER_UNREACHABLE")
            && error["hint"].as_str().is_some_and(|h| !h.is_empty())
            && took < Duration::from_secs(15),
        format!("{failed:?} after {} ms", took.as_millis()),
    );
}

/// The S1 group's coordinator changes behind the daemon's back (the
/// household's topology events are dropped, so its model is stale). A
/// group-volume change through the API hits UPnP 800 at the old
/// coordinator, follows the new one, and lands there.
fn check_reelect(s: &mut Scenario, api: &str) {
    let kitchen_uuid = s
        .sim_handle()
        .and_then(|sim| sim.player("Kitchen").map(|p| p.uuid.clone()))
        .unwrap_or_default();
    let joined = join_group(&s.lan(), s.ip("Office"), &PlayerId(kitchen_uuid.clone()));
    let grouped = joined.is_ok()
        && eventually(Duration::from_secs(3), || {
            get(api, "/zones").as_array().is_some_and(|z| z.len() == 3)
        });
    s.check(
        "reelect",
        "sim",
        "the Office joins the Kitchen again",
        grouped,
        format!("{joined:?}"),
    );

    // Silence the S1 household's events so the daemon misses the change.
    let silenced: Vec<String> = ["Kitchen", "Office"].map(String::from).to_vec();
    for room in &silenced {
        let _ = s
            .sim_handle()
            .map(|sim| sim.drop_notifies(room, Some(NotifyDrop::Next(10_000))));
    }
    let moved = s.sim_handle().map(|sim| sim.reelect_coordinator("Kitchen"));
    s.check(
        "reelect",
        "sim",
        "the group re-elects its coordinator",
        matches!(moved, Some(Ok(()))),
        format!("{moved:?}"),
    );

    let body = json!({ "zone": "Office", "volume": 23, "group": true }).to_string();
    let changed = http(
        api,
        "POST",
        "/volume",
        &[("Content-Type", "application/json")],
        &body,
    );
    let zgs = get_zone_group_state(&s.lan(), s.ip("Kitchen")).ok();
    let coordinator = zgs
        .as_ref()
        .and_then(|z| {
            z.groups
                .iter()
                .find(|g| g.members.iter().any(|m| m.uuid.0 == kitchen_uuid))
        })
        .map_or_default(|g| g.coordinator.0.clone());
    let room = s
        .sim_handle()
        .and_then(|sim| {
            sim.players()
                .iter()
                .find(|p| p.uuid == coordinator)
                .map(|p| p.room.clone())
        })
        .unwrap_or_default();
    let level = get_group_volume(&s.lan(), s.ip(&room)).ok();
    s.check(
        "reelect",
        "http",
        "the group-volume change follows the new coordinator (a HEALED note) and lands there",
        changed
            .as_ref()
            .is_ok_and(|(code, _, body)| *code == 200 && body.contains("HEALED"))
            && coordinator != kitchen_uuid
            && level == Some(23),
        format!("{changed:?}; new coordinator {room}; group volume {level:?}"),
    );
    for room in &silenced {
        let _ = s.sim_handle().map(|sim| sim.drop_notifies(room, None));
    }
}

/// The Kitchen moves to `spare` (a DHCP lease change) while the S1
/// household's events are silenced, so the daemon still has its old address,
/// which now refuses connections. A volume command through the daemon finds
/// it at the new one and lands there (a HEALED note); the next goes there at
/// once; a `--direct` run, which surveys afresh, finds it too.
fn check_moved(s: &mut Scenario, spare: (Ipv4Addr, SocketAddr)) {
    let silenced = ["Kitchen", "Office", "Bridge"];
    for room in silenced {
        let _ = s
            .sim_handle()
            .map(|sim| sim.drop_notifies(room, Some(NotifyDrop::Next(10_000))));
    }
    let (to, at) = spare;
    let moved = s.sim_handle_mut().map(|sim| sim.move_to("Kitchen", to, at));
    s.check(
        "moved",
        "sim",
        "the Kitchen moves to a new address; its old one refuses connections",
        matches!(moved, Some(Ok(_))),
        format!("{moved:?} -> {to} at {at}"),
    );
    let to = IpAddr::V4(to);
    for (step, level, assertion) in [
        (
            "moved-daemon",
            31,
            "a volume command through the daemon finds the Kitchen at its new address and lands \
             there (a HEALED note)",
        ),
        (
            "moved-again",
            32,
            "the next one goes to the new address at once (no HEALED note)",
        ),
    ] {
        let run = s.cli(
            step,
            &[
                "--daemon",
                "--json",
                "volume",
                "Kitchen",
                &level.to_string(),
            ],
        );
        let now = get_volume(&s.lan(), to).ok();
        let healed = run.stdout.contains("HEALED");
        s.check(
            "moved",
            "cli",
            assertion,
            run.ok() && healed == (step == "moved-daemon") && now == Some(level),
            format!("{run:?}; volume at {to}: {now:?}"),
        );
    }
    let run = s.cli(
        "moved-direct",
        &["--direct", "--json", "volume", "Kitchen", "33"],
    );
    let now = get_volume(&s.lan(), to).ok();
    s.check(
        "moved",
        "cli",
        "a --direct run, which surveys afresh, finds it at the new address too",
        run.ok() && now == Some(33),
        format!("{run:?}; volume at {to}: {now:?}"),
    );
    for room in silenced {
        let _ = s.sim_handle().map(|sim| sim.drop_notifies(room, None));
    }
}
