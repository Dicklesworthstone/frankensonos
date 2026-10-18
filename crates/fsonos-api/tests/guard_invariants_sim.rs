//! The guard's promises under random use (p7vq.2). Per seed: a random house
//! policy (caps, step limits, a room cap, quiet hours, deny and allow lists,
//! capped flags) and a random hour, a house left in a random state (rooms
//! loud or quiet, playing or paused), then random mutating calls by random
//! clients through the same `Surface` the HTTP API, MCP and CLI use, against
//! `fsonos-sim`. After every call the players' state, not the response, is
//! checked:
//!
//! * I1: a capped client never leaves a room above its cap there that it
//!   raised or started music in (a play, a resume, a join, a scene): the owner
//!   may set any level; an agent may lower one.
//! * I2: a denied call changes nothing.
//! * I3: a call that changed the house logged a row naming its client.
//! * I4: I1 holds for an undo too.
//! * I5: a client denied `set_volume` raises no room's volume by any route
//!   (a fade, a scene, an undo).
//!
//! A failure prints the seed, the policy and the steps so far as JSON lines.
//! `FSONOS_INVARIANT_SEED=n` replays one seed; `FSONOS_INVARIANT_SEEDS=n`
//! runs seeds 1..=n (default 12).

use chrono::{DateTime, FixedOffset};
use fsonos_api::plan::{
    TransportAction, plan_group, plan_mute, plan_play, plan_transport_set, plan_ungroup_set,
    plan_volume, plan_volume_set,
};
use fsonos_api::surface::scenes::SceneDefinition;
use fsonos_api::surface::schedules::FadeVolumeRequest;
use fsonos_api::surface::{Surface, Survey};
use fsonos_api::{
    ErrorCode, Failure, GroupRequest, MuteRequest, PlayRequest, VolumeRequest, ZoneRequest,
};
use fsonos_core::clock::FakeClock;
use fsonos_core::policy::{Client, Decision, Policy, ToolCall};
use fsonos_core::store::{ActionFilter, MemStore};
use fsonos_core::{HouseholdState, control, resolve_room};
use fsonos_proto::control::{get_transport_info, get_volume};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use serde_json::json;
use std::collections::BTreeMap;
use std::fmt::Write as _;

const ROOMS: [&str; 3] = ["Kitchen", "Office", "Den"];
const STREAM: &str = "x-rincon-mp3radio://stream.example.invalid/house.mp3";
const OTHER: &str = "x-rincon-mp3radio://stream.example.invalid/other.mp3";
const STEPS: usize = 30;

/// xorshift64*: small, deterministic, good enough to pick moves.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn draw(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.draw() % u64::try_from(n).unwrap()).unwrap()
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.draw() % 100 < percent
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

/// A room as its players report it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RoomState {
    volume: u8,
    /// Its group's transport state.
    state: String,
    /// The room leading its group.
    lead: &'static str,
}

impl RoomState {
    fn playing(&self) -> bool {
        self.state == "Playing"
    }
}

type House = BTreeMap<&'static str, RoomState>;

fn households(sim: &SimHandle) -> Vec<HouseholdState> {
    let mut st = HouseholdState::default();
    st.apply_topology(
        &get_zone_group_state(&sim.lan(), sim.player("Kitchen").unwrap().ip).unwrap(),
    );
    vec![st]
}

fn house(sim: &SimHandle) -> House {
    let lan = sim.lan();
    let houses = households(sim);
    let player = |room: &str| resolve_room(&houses, room).unwrap().player.clone();
    ROOMS
        .iter()
        .map(|&room| {
            let me = player(room);
            let coordinator = houses[0].coordinator_of(&me.id).unwrap_or(&me.id);
            let lead = *ROOMS
                .iter()
                .find(|r| player(r).id == *coordinator)
                .unwrap_or(&room);
            let state = get_transport_info(&lan, player(lead).ip)
                .map_or_else(|e| e.to_string(), |t| format!("{:?}", t.state));
            let volume = get_volume(&lan, me.ip).unwrap_or(255);
            (
                room,
                RoomState {
                    volume,
                    state,
                    lead,
                },
            )
        })
        .collect()
}

/// A random policy: the caps and step limits the guard enforces, quiet
/// hours, and which client may do what.
fn policy(rng: &mut Rng) -> String {
    let mut toml = format!(
        "[defaults]\nmax_volume = {}\nmax_step = {}\n",
        rng.pick(&[30, 40, 50, 60, 70]),
        rng.pick(&[5, 10, 20, 40])
    );
    if rng.chance(50) {
        let _ = write!(
            toml,
            "\n[quiet_hours]\nstart = \"22:00\"\nend = \"07:00\"\nmax_volume = {}\n",
            rng.pick(&[10, 20, 25])
        );
    }
    if rng.chance(50) {
        let _ = write!(
            toml,
            "\n[rooms.\"Office\"]\nmax_volume = {}\n",
            rng.pick(&[20, 35, 45, 60])
        );
    }
    let writes = [
        "set_volume",
        "volume",
        "play",
        "pause",
        "resume",
        "playback",
        "group",
        "grouping",
        "mute",
        "apply_scene",
        "undo_last",
        "undo",
        "fade_volume",
    ];
    let denied: Vec<String> = writes
        .iter()
        .filter(|_| rng.chance(25))
        .map(|w| format!("\"{w}\""))
        .collect();
    let _ = write!(
        toml,
        "\n[clients.\"tag:agent\"]\ndeny = [{}]\n",
        denied.join(", ")
    );
    if rng.chance(50) {
        let allowed: Vec<String> = [
            "read",
            "set_volume",
            "play",
            "pause",
            "resume",
            "apply_scene",
            "undo_last",
        ]
        .iter()
        .filter(|_| rng.chance(50))
        .map(|w| format!("\"{w}\""))
        .collect();
        let _ = write!(
            toml,
            "\n[clients.\"guest@example.com\"]\nallow = [{}]\n",
            allowed.join(", ")
        );
    }
    let _ = write!(
        toml,
        "\n[clients.\"loopback-http\"]\ncapped = {}\n",
        rng.chance(70)
    );
    if rng.chance(20) {
        toml += "\n[clients.cli]\ncapped = true\n";
    }
    toml
}

fn clients() -> Vec<Client> {
    vec![
        Client::Cli,
        Client::McpStdio,
        Client::LoopbackHttp,
        Client::Tailnet("tag:agent".into()),
        Client::Tailnet("guest@example.com".into()),
        Client::Unknown,
    ]
}

fn volume(zone: &str, volume: Option<i64>, delta: Option<i64>) -> VolumeRequest {
    VolumeRequest {
        zone: zone.into(),
        volume,
        delta,
        group: false,
    }
}

/// One random call by `client`: what it was, and whether the guard denied it.
fn act(s: &Surface, rng: &mut Rng, client: &Client) -> (String, Result<(), ErrorCode>) {
    let room = *rng.pick(&ROOMS);
    let level = i64::try_from(rng.below(101)).unwrap();
    let zone = |z: &str| ZoneRequest { zone: z.into() };
    let (what, result) = match rng.below(12) {
        which @ 0..4 => volume_call(s, rng, client, which, room, level),
        4 => {
            let action = if rng.chance(50) {
                TransportAction::Pause
            } else {
                TransportAction::Resume
            };
            let tool = if action == TransportAction::Pause {
                "pause"
            } else {
                "resume"
            };
            (
                format!("{tool} {room}"),
                s.control_set(client, tool, |h| plan_transport_set(h, &zone(room), action))
                    .map(drop),
            )
        }
        5 => {
            let req = PlayRequest {
                zone: room.into(),
                source_uri: (*rng.pick(&[STREAM, OTHER])).into(),
                title: None,
            };
            (
                format!("play {room} {}", req.source_uri),
                s.control(client, "play", |h| plan_play(h, &req)).map(drop),
            )
        }
        6 => {
            let to = *rng.pick(&ROOMS);
            let req = GroupRequest {
                zone: room.into(),
                to: to.into(),
            };
            (
                format!("group {room} with {to}"),
                s.control(client, "group", |h| plan_group(h, &req))
                    .map(drop),
            )
        }
        7 => (
            format!("ungroup {room}"),
            s.control_set(client, "ungroup", |h| plan_ungroup_set(h, &zone(room)))
                .map(drop),
        ),
        8 | 9 => {
            let scene = *rng.pick(&["loud", "evening"]);
            (
                format!("apply_scene {scene}"),
                s.apply_scene(client, scene).map(drop),
            )
        }
        10 => (
            "undo_last".into(),
            s.undo(client, *client != Client::Cli).map(drop),
        ),
        _ => {
            let req = MuteRequest {
                zone: room.into(),
                mute: rng.chance(50),
            };
            (
                format!("mute {room} {}", req.mute),
                s.control(client, "mute", |h| plan_mute(h, &req)).map(drop),
            )
        }
    };
    (what, result.map_err(|f| f.code))
}

/// A volume call: `which` picks a level for a room, a step, a level for
/// every room, or a fade.
fn volume_call(
    s: &Surface,
    rng: &mut Rng,
    client: &Client,
    which: usize,
    room: &str,
    level: i64,
) -> (String, Result<(), Failure>) {
    match which {
        0 => (
            format!("set_volume {room} {level}"),
            s.control(client, "set_volume", |h| {
                plan_volume(h, &volume(room, Some(level), None))
            })
            .map(drop),
        ),
        1 => {
            let by = *rng.pick(&[-30, -10, 5, 15, 30, 60]);
            (
                format!("set_volume {room} by {by}"),
                s.control(client, "set_volume", |h| {
                    plan_volume(h, &volume(room, None, Some(by)))
                })
                .map(drop),
            )
        }
        2 => (
            format!("set_volume all {level}"),
            s.control_set(client, "set_volume", |h| {
                plan_volume_set(h, &volume("all", Some(level), None))
            })
            .map(drop),
        ),
        _ => (
            format!("fade_volume {room} {level}"),
            s.fade_volume_to(
                client,
                &FadeVolumeRequest {
                    zone: room.into(),
                    volume: level,
                    fade_secs: 0,
                },
            )
            .map(drop),
        ),
    }
}

/// One call, judged.
struct Step<'a> {
    client: &'a Client,
    before: &'a House,
    after: &'a House,
    result: &'a Result<(), ErrorCode>,
    /// The call logged a row naming its client.
    logged: bool,
}

/// The broken invariant after `step`, if any.
fn violation(policy: &Policy, now: DateTime<FixedOffset>, step: &Step<'_>) -> Option<String> {
    let Step {
        client,
        before,
        after,
        result,
        logged,
    } = step;
    if **result == Err(ErrorCode::PolicyDenied) && before != after {
        return Some("I2: a denied call changed the house".into());
    }
    let set_volume = ToolCall {
        name: "set_volume",
        read_only: false,
    };
    let no_volume = matches!(policy.authorize(set_volume, client), Decision::Deny { .. });
    for (room, now_is) in *after {
        let was = &before[room];
        let raised = now_is.volume > was.volume;
        if raised && no_volume {
            return Some(format!(
                "I5: {client}, denied set_volume, raised {room} {} -> {}",
                was.volume, now_is.volume
            ));
        }
        // Music starts in a room that begins playing in the same group, or
        // that joins another room's playing group. One that left its group
        // to stand alone is not counted: joining replaced its own source
        // with the group's (x-rincon:), so a player leaves with nothing to
        // play, where the sim resumes what it played before it joined.
        let joined = was.lead != now_is.lead && now_is.lead != *room;
        let started = now_is.playing() && ((!was.playing() && was.lead == now_is.lead) || joined);
        if (raised || started)
            && let Some((cap, why)) = policy.cap_for(room, client, now)
            && now_is.volume > cap
        {
            let how = if raised { "raised" } else { "started music in" };
            return Some(format!(
                "I1/I4: {client} {how} {room}, now at {} (was {}): above its cap ({why})",
                now_is.volume, was.volume
            ));
        }
    }
    if before != after && !logged {
        return Some("I3: the house changed and no action-log row names the client".into());
    }
    None
}

fn surface(sim: &SimHandle, policy: Policy, now: DateTime<FixedOffset>) -> Surface {
    let kitchen = sim.player("Kitchen").unwrap().ip;
    let survey: Survey = Box::new(move |t| {
        let mut st = HouseholdState::default();
        st.apply_topology(&get_zone_group_state(t, kitchen).map_err(fsonos_core::CoreError::from)?);
        Ok(vec![st])
    });
    Surface::new(
        Box::new(sim.lan()),
        survey,
        policy,
        Box::new(FakeClock::new(now)),
    )
    .with_action_log(Box::new(MemStore::default()), "mcp")
}

/// The owner's evening: every room playing loud, saved as "loud"; Kitchen
/// and Office together on a station, no levels, saved as "evening"; then
/// each room left loud or quiet, playing or paused.
fn set_the_scene(sim: &SimHandle, s: &Surface, rng: &mut Rng) {
    let lan = sim.lan();
    let houses = households(sim);
    let id = |room: &str| resolve_room(&houses, room).unwrap().coordinator.id.clone();
    for room in ROOMS {
        control::play_uri(&lan, &houses, &id(room), STREAM, "").unwrap();
        control::set_volume(&lan, &houses, &id(room), 90).unwrap();
    }
    s.save_scene(&Client::Cli, "loud").unwrap();
    let evening: SceneDefinition = serde_json::from_value(json!({
        "groups": [{"coordinator": "Kitchen", "members": ["Office"],
            "source": {"kind": "uri", "uri": STREAM}}]
    }))
    .unwrap();
    s.save_scene_from(&Client::Cli, "evening", &evening)
        .unwrap();
    for room in ROOMS {
        control::set_volume(&lan, &houses, &id(room), *rng.pick(&[15, 40, 90])).unwrap();
        if rng.chance(40) {
            control::pause(&lan, &houses, &id(room)).unwrap();
        }
    }
}

/// One seed's run: `Err` with the trace when an invariant broke.
fn run(seed: u64) -> Result<(), String> {
    let mut rng = Rng::new(seed);
    let toml = policy(&mut rng);
    let policy = Policy::from_toml(&toml).unwrap_or_else(|e| panic!("{e}\n{toml}"));
    let hour = *rng.pick(&["02:30", "14:30"]);
    let now = DateTime::parse_from_rfc3339(&format!("2026-01-15T{hour}:00+00:00")).unwrap();
    let sim = SimHousehold::builder()
        .s1(ROOMS.map(|r| SimPlayerSpec::new(r, SimModel::Play5Gen1)))
        .spawn()
        .unwrap();
    let s = surface(&sim, policy.clone(), now);
    set_the_scene(&sim, &s, &mut rng);
    let all = clients();
    let log = || {
        s.recent_actions(&Client::Cli, &ActionFilter::default())
            .unwrap()
    };
    let mut trace = vec![json!({"seed": seed, "at": now.to_rfc3339(), "policy": toml}).to_string()];
    for n in 0..STEPS {
        let client = rng.pick(&all).clone();
        let rows_before = log().len();
        let before = house(&sim);
        let (call, result) = act(&s, &mut rng, &client);
        let after = house(&sim);
        let rows = log();
        let new_rows = rows.len().saturating_sub(rows_before);
        let logged = rows
            .iter()
            .take(new_rows)
            .any(|r| r.action.client == client.key());
        let rooms: BTreeMap<_, _> = after
            .iter()
            .map(|(room, r)| (room, json!([r.volume, r.state, r.lead])))
            .collect();
        trace.push(
            json!({"step": n, "client": client.key(), "call": call,
                "result": format!("{result:?}"), "rooms": rooms})
            .to_string(),
        );
        let step = Step {
            client: &client,
            before: &before,
            after: &after,
            result: &result,
            logged,
        };
        if let Some(broken) = violation(&policy, now, &step) {
            sim.shutdown();
            return Err(format!("{broken}\n{}", trace.join("\n")));
        }
    }
    sim.shutdown();
    Ok(())
}

#[test]
fn random_calls_by_random_clients_keep_every_guard_promise() {
    let seeds: Vec<u64> = if let Ok(one) = std::env::var("FSONOS_INVARIANT_SEED") {
        vec![one.parse().expect("FSONOS_INVARIANT_SEED is a number")]
    } else {
        let n = std::env::var("FSONOS_INVARIANT_SEEDS")
            .ok()
            .and_then(|n| n.parse().ok())
            .unwrap_or(12);
        (1..=n).collect()
    };
    for seed in seeds {
        if let Err(trace) = run(seed) {
            panic!("guard invariant broken; replay with FSONOS_INVARIANT_SEED={seed}\n{trace}");
        }
    }
}
