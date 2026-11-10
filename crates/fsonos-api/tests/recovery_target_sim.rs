//! Recovery follows the room the caller named, even when the old leader
//! joins an unrelated group. Exercise both generations over real loopback
//! SOAP, including the caps and history of the recovered playback audience.

use fsonos_api::plan::{plan_play, plan_volume};
use fsonos_api::surface::{Surface, Survey};
use fsonos_api::{ErrorCode, NoteCode, PlayRequest, VolumeRequest};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::MemStore;
use fsonos_core::{HouseholdState, control, resolve_room};
use fsonos_proto::control::{get_group_volume, get_volume, set_volume};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use std::net::IpAddr;

const STREAM: &str = "x-rincon-mp3radio://stream.example.invalid/recovered.mp3";

fn house(s2: bool) -> SimHandle {
    let model = if s2 {
        SimModel::One
    } else {
        SimModel::Play5Gen1
    };
    let rooms = ["Kitchen", "Office", "Den", "Bedroom"].map(|name| SimPlayerSpec::new(name, model));
    let builder = SimHousehold::builder();
    if s2 {
        builder.s2(rooms)
    } else {
        builder.s1(rooms)
    }
    .spawn()
    .unwrap()
}

fn now(sim: &SimHandle) -> Vec<HouseholdState> {
    let mut house = HouseholdState::default();
    house.apply_topology(
        &get_zone_group_state(&sim.lan(), sim.player("Office").unwrap().ip).unwrap(),
    );
    vec![house]
}

fn join(sim: &SimHandle, member: &str, leader: &str) {
    let houses = now(sim);
    let member = resolve_room(&houses, member).unwrap().player.id.clone();
    let leader = resolve_room(&houses, leader)
        .unwrap()
        .coordinator
        .id
        .clone();
    control::join(&sim.lan(), &houses, &member, &leader).unwrap();
}

fn surface_over(sim: &SimHandle, transport: Box<dyn Transport + Send + Sync>) -> Surface {
    // Office's address stays stable while Kitchen moves. Its topology also
    // supplies a genuinely fresh survey for the changed-address path.
    let seed = sim.player("Office").unwrap().ip;
    let survey: Survey = Box::new(move |t| {
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(t, seed).map_err(fsonos_core::CoreError::from)?);
        Ok(vec![house])
    });
    Surface::new(
        transport,
        survey,
        Policy::from_toml("[defaults]\nmax_volume = 30\nmax_step = 100\n").unwrap(),
        Box::new(SystemClock),
    )
    .with_action_log(Box::new(MemStore::default()), "test")
}

fn surface(sim: &SimHandle) -> Surface {
    surface_over(sim, Box::new(sim.lan()))
}

fn prepare(sim: &SimHandle, surface: &Surface) {
    join(sim, "Office", "Kitchen");
    let cached = surface.households().unwrap();
    assert_eq!(
        resolve_room(&cached, "Office")
            .unwrap()
            .coordinator
            .room_name,
        "Kitchen"
    );
    // Office stays behind. The old leader is now a member of Den's group:
    // a retry anchored on Kitchen would control Den instead of Office.
    join(sim, "Kitchen", "Den");
    let current = now(sim);
    assert_eq!(
        resolve_room(&current, "Office")
            .unwrap()
            .coordinator
            .room_name,
        "Office"
    );
    assert_eq!(
        resolve_room(&current, "Kitchen")
            .unwrap()
            .coordinator
            .room_name,
        "Den"
    );
}

fn volume() -> VolumeRequest {
    VolumeRequest {
        zone: "Office".into(),
        volume: Some(31),
        delta: None,
        group: true,
    }
}

fn play() -> PlayRequest {
    PlayRequest {
        zone: "Office".into(),
        source_uri: STREAM.into(),
        title: None,
    }
}

#[test]
fn coordinator_recovery_stays_with_the_requested_room_when_the_old_leader_leaves() {
    for s2 in [false, true] {
        let sim = house(s2);
        let surface = surface(&sim);
        prepare(&sim, &surface);
        let lan = sim.lan();
        let den = sim.player("Den").unwrap().ip;
        let before = get_group_volume(&lan, den).unwrap();
        let mark = sim.soap_log().len();
        let outcome = surface
            .control(&Client::Cli, "set_volume", |h| plan_volume(h, &volume()))
            .unwrap();
        assert_eq!(
            get_group_volume(&lan, sim.player("Office").unwrap().ip).unwrap(),
            31
        );
        assert_eq!(get_group_volume(&lan, den).unwrap(), before);
        let log = sim.soap_log();
        let writes: Vec<_> = log[mark..]
            .iter()
            .filter(|entry| entry.action == "SetGroupVolume")
            .map(|entry| (entry.room.as_str(), entry.result.is_ok()))
            .collect();
        assert_eq!(writes, [("Office", true)]);
        let attempts: Vec<_> = log[mark..]
            .iter()
            .filter(|entry| entry.action == "SnapshotGroupVolume")
            .map(|entry| (entry.room.as_str(), entry.result.is_ok()))
            .collect();
        assert_eq!(attempts, [("Kitchen", false), ("Office", true)]);
        assert!(outcome.notes.iter().any(|note| {
            note.code == NoteCode::Healed && note.detail.contains("Office's group")
        }));
    }
}

#[test]
fn address_recovery_rebinds_the_requested_room_after_simultaneous_regrouping() {
    for s2 in [false, true] {
        let mut sim = house(s2);
        let surface = surface(&sim);
        prepare(&sim, &surface);
        let new_address = sim.change_address("Kitchen").unwrap();
        let mark = sim.soap_log().len();
        let outcome = surface
            .control(&Client::Cli, "set_volume", |h| plan_volume(h, &volume()))
            .unwrap();
        assert_eq!(
            get_group_volume(&sim.lan(), sim.player("Office").unwrap().ip).unwrap(),
            31
        );
        let log = sim.soap_log();
        let writes: Vec<_> = log[mark..]
            .iter()
            .filter(|entry| entry.action == "SetGroupVolume")
            .map(|entry| entry.room.as_str())
            .collect();
        assert_eq!(writes, ["Office"], "{log:?}");
        let note = outcome
            .notes
            .iter()
            .find(|note| note.code == NoteCode::Healed)
            .unwrap();
        assert!(note.detail.contains(&new_address.to_string()), "{note:?}");
        assert!(note.detail.ends_with("sent to Office"), "{note:?}");
    }
}

fn loud_new_audience(sim: &SimHandle) {
    join(sim, "Bedroom", "Office");
    let lan = sim.lan();
    for (room, level) in [
        ("Kitchen", 10),
        ("Office", 10),
        ("Den", 80),
        ("Bedroom", 90),
    ] {
        set_volume(&lan, sim.player(room).unwrap().ip, level).unwrap();
    }
}

fn assert_recovered_play(sim: &SimHandle, surface: &Surface, mark: usize) {
    let lan = sim.lan();
    for (room, level) in [
        ("Kitchen", 10),
        ("Office", 10),
        ("Den", 80),
        ("Bedroom", 30),
    ] {
        assert_eq!(
            get_volume(&lan, sim.player(room).unwrap().ip).unwrap(),
            level,
            "{room}"
        );
    }
    let log = sim.soap_log();
    let calls = &log[mark..];
    let lowered = calls
        .iter()
        .position(|entry| entry.room == "Bedroom" && entry.action == "SetVolume")
        .unwrap();
    let played = calls
        .iter()
        .position(|entry| entry.room == "Office" && entry.action == "Play")
        .unwrap();
    assert!(
        lowered < played,
        "new members must be capped before playback: {calls:?}"
    );
    assert!(
        !calls.iter().any(|entry| {
            entry.room == "Den"
                && matches!(
                    entry.action.as_str(),
                    "SetVolume" | "SetAVTransportURI" | "Play"
                )
        }),
        "unrelated group was changed: {calls:?}"
    );
    let plays = surface
        .recent_plays(&Client::Cli, Some("Office"), 10)
        .unwrap();
    assert_eq!(plays.len(), 1, "{plays:?}");
    assert_eq!(plays[0].source_uri, STREAM);
    assert_eq!(plays[0].room.as_deref(), Some("Office"));
    assert!(
        surface
            .recent_plays(&Client::Cli, Some("Den"), 10)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn coordinator_recovery_caps_new_members_and_records_the_actual_playing_group() {
    for s2 in [false, true] {
        let sim = house(s2);
        let surface = surface(&sim);
        prepare(&sim, &surface);
        loud_new_audience(&sim);
        // SetAVTransportURI can itself ungroup a member. This scenario is
        // specifically the initial 800 recovery path, not an accepted write.
        sim.upnp_fault("Kitchen", "SetAVTransportURI", 800).unwrap();
        let mark = sim.soap_log().len();
        let outcome = surface
            .control(&Client::McpStdio, "play", |h| plan_play(h, &play()))
            .unwrap();
        assert!(outcome.notes.iter().any(|note| {
            note.code == NoteCode::VolumeClamped && note.detail.contains("Bedroom")
        }));
        assert_recovered_play(&sim, &surface, mark);
    }
}

#[test]
fn recovery_during_initial_cap_reads_rebinds_before_starting_playback() {
    for s2 in [false, true] {
        let mut sim = house(s2);
        let surface = surface(&sim);
        prepare(&sim, &surface);
        loud_new_audience(&sim);
        sim.change_address("Kitchen").unwrap();
        let mark = sim.soap_log().len();
        surface
            .control(&Client::McpStdio, "play", |h| plan_play(h, &play()))
            .unwrap();
        assert_recovered_play(&sim, &surface, mark);
    }
}

struct NoVolumeAt {
    lan: SimLan,
    host: IpAddr,
}

impl Transport for NoVolumeAt {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if host == self.host && action.contains("#GetVolume") {
            return Err(ProtoError::Network {
                target: host.to_string(),
                detail: "new group member's volume is unreadable".into(),
            });
        }
        self.lan.soap_post(host, path, action, body)
    }
}

#[test]
fn an_unreadable_new_member_prevents_recovered_playback() {
    let sim = house(true);
    let surface = surface_over(
        &sim,
        Box::new(NoVolumeAt {
            lan: sim.lan(),
            host: sim.player("Bedroom").unwrap().ip,
        }),
    );
    prepare(&sim, &surface);
    loud_new_audience(&sim);
    sim.upnp_fault("Kitchen", "SetAVTransportURI", 800).unwrap();
    let mark = sim.soap_log().len();
    let failure = surface
        .control(&Client::McpStdio, "play", |h| plan_play(h, &play()))
        .unwrap_err();
    assert_eq!(failure.code, ErrorCode::PlayerUnreachable, "{failure:?}");
    assert!(failure.detail.contains("Bedroom"), "{failure:?}");
    let log = sim.soap_log();
    assert!(
        !log[mark..].iter().any(|entry| {
            entry.result.is_ok()
                && matches!(
                    entry.action.as_str(),
                    "SetAVTransportURI" | "Play" | "SetVolume"
                )
        }),
        "no recovered playback or cap write may precede the failed cap read: {:?}",
        &log[mark..]
    );
    assert!(
        surface
            .recent_plays(&Client::Cli, None, 10)
            .unwrap()
            .is_empty()
    );
}
