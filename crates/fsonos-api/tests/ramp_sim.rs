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
    let s = Surface::new(
        Box::new(sim.lan()),
        survey(),
        Policy::default(),
        Box::new(SystemClock),
    )
    .with_action_log(Box::new(MemStore::default()), "mcp");
    let set = VolumeRequest {
        zone: "Kitchen".into(),
        volume: Some(10),
        delta: None,
        group: false,
    };
    s.control(&Client::Cli, "set_volume", |h| plan_volume(h, &set))
        .unwrap();
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
