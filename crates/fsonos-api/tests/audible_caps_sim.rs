//! Music a capped caller makes audible is held to its caps however it
//! starts (p7vq.13), through `Surface` against `fsonos-sim` players over
//! real sockets: an unmute brings a loud room down to the cap first, and a
//! play in a room whose level cannot be read is refused before anything is
//! sent.

use fsonos_api::plan::{plan_mute, plan_play};
use fsonos_api::surface::{Surface, Survey};
use fsonos_api::{ErrorCode, MuteRequest, NoteCode, PlayRequest};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::MemStore;
use fsonos_core::{HouseholdState, resolve_room};
use fsonos_proto::control::{get_mute, get_volume, set_mute, set_volume};
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan};
use std::net::IpAddr;
use std::time::Duration;

const STREAM: &str = "x-rincon-mp3radio://stream.example.invalid/loud.mp3";

fn house(sim: &SimHandle) -> Vec<HouseholdState> {
    fsonos_core::inventory::survey(&sim.lan(), &[], Duration::from_millis(500))
        .unwrap()
        .households
}

/// Every client but the CLI is capped at 30 here.
fn surface(transport: Box<dyn Transport + Send + Sync>, houses: Vec<HouseholdState>) -> Surface {
    let survey: Survey = Box::new(move |_| Ok(houses.clone()));
    Surface::new(
        transport,
        survey,
        Policy::from_toml("[defaults]\nmax_volume = 30\nmax_step = 100\n").unwrap(),
        Box::new(SystemClock),
    )
}

fn play(zone: &str) -> PlayRequest {
    PlayRequest {
        zone: zone.into(),
        source_uri: STREAM.into(),
        title: None,
    }
}

fn unmute(zone: &str) -> MuteRequest {
    MuteRequest {
        zone: zone.into(),
        mute: false,
    }
}

/// The owner leaves Kitchen playing at 80, muted; an agent unmutes it. It
/// sounds at 30, not 80, and the answer says it was lowered.
#[test]
fn a_capped_unmute_brings_a_loud_room_to_its_cap_first() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let houses = house(&sim);
    let kitchen = resolve_room(&houses, "Kitchen").unwrap().player.ip;
    let s = surface(Box::new(sim.lan()), houses);
    s.control(&Client::Cli, "play", |h| plan_play(h, &play("Kitchen")))
        .unwrap();
    set_volume(&sim.lan(), kitchen, 80).unwrap();
    set_mute(&sim.lan(), kitchen, true).unwrap();

    let out = s
        .control(&Client::McpStdio, "mute", |h| {
            plan_mute(h, &unmute("Kitchen"))
        })
        .unwrap();
    assert!(!get_mute(&sim.lan(), kitchen).unwrap());
    assert_eq!(get_volume(&sim.lan(), kitchen).unwrap(), 30, "{out:?}");
    assert!(
        out.notes
            .iter()
            .any(|n| n.code == NoteCode::VolumeClamped && n.detail.contains("from 80 to 30")),
        "{:?}",
        out.notes
    );

    // The owner's CLI runs uncapped: its unmute leaves the level alone.
    set_volume(&sim.lan(), kitchen, 80).unwrap();
    set_mute(&sim.lan(), kitchen, true).unwrap();
    s.control(&Client::Cli, "mute", |h| plan_mute(h, &unmute("Kitchen")))
        .unwrap();
    assert_eq!(get_volume(&sim.lan(), kitchen).unwrap(), 80);
    sim.shutdown();
}

/// Passes everything to the sim, but no GetVolume to `host` is answered.
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
                detail: "injected unreadable volume".into(),
            });
        }
        self.lan.soap_post(host, path, action, body)
    }
}

/// A capped play into a room whose level cannot be read is refused with
/// nothing sent to the group, where it used to play there at whatever level
/// the room was left at. The CLI, uncapped, reads nothing and plays.
#[test]
fn a_capped_play_is_refused_where_a_room_level_cannot_be_read() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let houses = house(&sim);
    let kitchen = resolve_room(&houses, "Kitchen").unwrap().player.ip;
    let s = surface(
        Box::new(NoVolumeAt {
            lan: sim.lan(),
            host: kitchen,
        }),
        houses,
    );
    let mark = sim.soap_log().len();
    let refused = s
        .control(&Client::McpStdio, "play", |h| {
            plan_play(h, &play("Kitchen"))
        })
        .unwrap_err();
    assert_eq!(refused.code, ErrorCode::PlayerUnreachable, "{refused:?}");
    assert!(refused.detail.contains("Kitchen"), "{refused:?}");
    let writes = sim.soap_log()[mark..]
        .iter()
        .filter(|e| !e.action.starts_with("Get") && e.action != "Browse")
        .count();
    assert_eq!(writes, 0, "{:?}", &sim.soap_log()[mark..]);

    s.control(&Client::Cli, "play", |h| plan_play(h, &play("Kitchen")))
        .unwrap();
    sim.shutdown();
}

/// A caller denied set_volume mutes a room left playing at 80, then undoes
/// it. Its volume restore is refused, and the unmute would make the room
/// sound at 80, above its cap of 30, so that is refused too: the room stays
/// muted, at the level it had.
#[test]
fn an_undo_cannot_unmute_a_room_above_the_callers_cap() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let houses = house(&sim);
    let kitchen = resolve_room(&houses, "Kitchen").unwrap().player.ip;
    let survey: Survey = Box::new(move |_| Ok(houses.clone()));
    let policy = Policy::from_toml(
        "[defaults]\nmax_volume = 30\nmax_step = 100\n\n[clients.\"mcp-stdio\"]\ndeny = [\"set_volume\"]\n",
    )
    .unwrap();
    let s = Surface::new(Box::new(sim.lan()), survey, policy, Box::new(SystemClock))
        .with_action_log(Box::new(MemStore::default()), "mcp");
    s.control(&Client::Cli, "play", |h| plan_play(h, &play("Kitchen")))
        .unwrap();
    set_volume(&sim.lan(), kitchen, 80).unwrap();
    let mute = MuteRequest {
        zone: "Kitchen".into(),
        mute: true,
    };
    s.control(&Client::McpStdio, "mute", |h| plan_mute(h, &mute))
        .unwrap();
    assert!(get_mute(&sim.lan(), kitchen).unwrap());

    let undone = s.undo(&Client::McpStdio, true);
    assert!(get_mute(&sim.lan(), kitchen).unwrap(), "{undone:?}");
    assert_eq!(get_volume(&sim.lan(), kitchen).unwrap(), 80, "{undone:?}");
    sim.shutdown();
}
