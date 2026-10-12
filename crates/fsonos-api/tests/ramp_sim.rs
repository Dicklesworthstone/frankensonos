//! `fade_volume` with no fade, against `fsonos-sim` players over real
//! sockets: each speaker ramps itself at its own pace (RampToVolume, no
//! stepped SetVolume) and the call answers without waiting for it, each room's level is
//! bounded as an immediate change would be, and a speaker that refuses
//! RampToVolume is stepped there at the same pace instead.

use fsonos_api::plan::plan_volume;
use fsonos_api::surface::schedules::FadeVolumeRequest;
use fsonos_api::surface::{Surface, Survey};
use fsonos_api::{NoteCode, VolumeRequest};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::MemStore;
use fsonos_core::{control, resolve_room};
use fsonos_sim::{SimHandle, SimHousehold};
use std::time::Duration;

fn survey() -> Survey {
    Box::new(|t| Ok(fsonos_core::inventory::survey(t, &[], Duration::from_millis(500))?.households))
}

/// `room`'s volume now.
fn level(sim: &SimHandle, room: &str) -> u8 {
    let lan = sim.lan();
    let houses = fsonos_core::inventory::survey(&lan, &[], Duration::from_millis(500))
        .unwrap()
        .households;
    let player = resolve_room(&houses, room).unwrap().player.id.clone();
    control::volume(&lan, &houses, &player).unwrap()
}

/// How many `action` requests `room` has had.
fn sent(sim: &SimHandle, room: &str, action: &str) -> usize {
    sim.soap_log()
        .iter()
        .filter(|e| e.room == room && e.action == action)
        .count()
}

fn surface(sim: &SimHandle) -> Surface {
    Surface::new(
        Box::new(sim.lan()),
        survey(),
        Policy::default(),
        Box::new(SystemClock),
    )
    .with_action_log(Box::new(MemStore::default()), "mcp")
}

/// Set `room`'s volume now, as the CLI.
fn set(s: &Surface, room: &str, level: i64) {
    let req = VolumeRequest {
        zone: room.into(),
        volume: Some(level),
        delta: None,
        group: false,
    };
    s.control(&Client::Cli, "set_volume", |h| plan_volume(h, &req))
        .unwrap();
}

/// `zone` to `volume` with no fade.
fn ramp(zone: &str, volume: i64) -> FadeVolumeRequest {
    FadeVolumeRequest {
        zone: zone.into(),
        volume,
        fade_secs: 0,
    }
}

#[test]
fn a_ramp_with_no_fade_lets_each_speaker_ramp_itself() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let s = surface(&sim);
    set(&s, "Kitchen", 10);
    let sets = sent(&sim, "Kitchen", "SetVolume");

    let out = s
        .fade_volume_to(&Client::Cli, &ramp("Kitchen", 30))
        .unwrap();
    assert!(
        out.done
            .starts_with("ramping Kitchen's volume to 30 at each speaker's own pace"),
        "{}",
        out.done
    );
    assert_eq!(level(&sim, "Kitchen"), 30);
    assert_eq!(sent(&sim, "Kitchen", "RampToVolume"), 1);
    assert_eq!(sent(&sim, "Kitchen", "SetVolume"), sets, "no stepped fade");

    // An agent's ramp goes no further than an immediate change would.
    let agent = Client::Tailnet("tag:agent".into());
    let out = s.fade_volume_to(&agent, &ramp("Kitchen", 100)).unwrap();
    let capped = level(&sim, "Kitchen");
    assert!(
        capped > 30 && capped <= 70,
        "within the agent's cap: {capped}, {out:?}"
    );
    assert!(
        out.notes.iter().any(|n| n.code == NoteCode::VolumeClamped),
        "{out:?}"
    );

    // A speaker that refuses RampToVolume is stepped there at its pace.
    sim.upnp_fault("Office", "RampToVolume", 402).unwrap();
    let from = level(&sim, "Office");
    let to = if from >= 2 { from - 2 } else { from + 2 };
    let office_sets = sent(&sim, "Office", "SetVolume");
    s.fade_volume_to(&Client::Cli, &ramp("Office", i64::from(to)))
        .unwrap();
    assert_eq!(level(&sim, "Office"), to);
    assert_eq!(sent(&sim, "Office", "RampToVolume"), 1);
    assert!(sent(&sim, "Office", "SetVolume") > office_sets, "stepped");
    sim.shutdown();
}

#[test]
fn a_ramp_takes_every_room_to_its_own_level_together() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let s = surface(&sim);
    set(&s, "Kitchen", 10);
    set(&s, "Office", 40);
    let from = sim.soap_log().len();
    // The agent's limits bound each room to a level of its own.
    let agent = Client::Tailnet("tag:agent".into());
    let ramp = FadeVolumeRequest {
        zone: "all".into(),
        volume: 100,
        fade_secs: 1,
    };
    let out = s.fade_volume_to(&agent, &ramp).unwrap();
    let (kitchen, office) = (level(&sim, "Kitchen"), level(&sim, "Office"));
    assert!(
        kitchen > 10 && office > 40 && kitchen != office,
        "{kitchen} {office}: {out:?}"
    );
    // One fade: each room's steps begin before the other's end, rather than
    // one level after the other.
    let log = sim.soap_log();
    let steps = |room: &str| -> Vec<usize> {
        log.iter()
            .enumerate()
            .skip(from)
            .filter(|(_, e)| e.room == room && e.action == "SetVolume")
            .map(|(i, _)| i)
            .collect()
    };
    let (k, o) = (steps("Kitchen"), steps("Office"));
    let (Some(k_last), Some(o_last)) = (k.last(), o.last()) else {
        panic!("both rooms ramp: {k:?} {o:?}");
    };
    assert!(k.len() > 1 && o.len() > 1, "{k:?} {o:?}");
    assert!(k[0] < *o_last && o[0] < *k_last, "together: {k:?} {o:?}");
    sim.shutdown();
}
