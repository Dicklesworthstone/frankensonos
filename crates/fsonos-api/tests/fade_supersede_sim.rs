//! A volume set other than by a fade ends the fade in flight there first: a
//! scene applied, or an undo, while a ramp runs keeps its level rather than
//! being overwritten a step later. Against `fsonos-sim` players over real
//! loopback sockets, with the daemon's fader.

use fsonos_api::VolumeRequest;
use fsonos_api::plan::plan_volume;
use fsonos_api::surface::schedules::{FadeVolumeRequest, Sleep};
use fsonos_api::surface::{Surface, Survey};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::MemStore;
use fsonos_core::{HouseholdState, control, resolve_room};
use fsonos_sim::{SimHandle, SimHousehold};
use std::time::Duration;

fn survey() -> Survey {
    Box::new(|t| Ok(fsonos_core::inventory::survey(t, &[], Duration::from_millis(500))?.households))
}

fn surface(sim: &SimHandle) -> Surface {
    Surface::new(
        Box::new(sim.lan()),
        survey(),
        Policy::default(),
        Box::new(SystemClock),
    )
    .with_action_log(Box::new(MemStore::default()), "cli")
    .with_sleep(Sleep::default())
}

fn house(sim: &SimHandle) -> Vec<HouseholdState> {
    fsonos_core::inventory::survey(&sim.lan(), &[], Duration::from_millis(500))
        .unwrap()
        .households
}

fn kitchen_volume(sim: &SimHandle) -> u8 {
    let houses = house(sim);
    let kitchen = resolve_room(&houses, "Kitchen").unwrap().player.id.clone();
    control::volume(&sim.lan(), &houses, &kitchen).unwrap()
}

fn set(s: &Surface, level: i64) {
    let req = VolumeRequest {
        zone: "Kitchen".into(),
        volume: Some(level),
        delta: None,
        group: false,
    };
    s.control(&Client::Cli, "set_volume", |h| plan_volume(h, &req))
        .unwrap();
}

/// Ramp the Kitchen to 60 over 4 s on a thread of its own while `meanwhile`
/// runs half a second in; the Kitchen's volume once the ramp has had time to
/// step on.
fn during_a_ramp(s: &Surface, sim: &SimHandle, meanwhile: impl FnOnce()) -> u8 {
    std::thread::scope(|scope| {
        let ramp = scope.spawn(|| {
            s.fade_volume_to(
                &Client::Cli,
                &FadeVolumeRequest {
                    zone: "Kitchen".into(),
                    volume: 60,
                    fade_secs: 4,
                },
            )
        });
        std::thread::sleep(Duration::from_millis(500));
        meanwhile();
        // Long enough for several more steps, were the ramp still going.
        std::thread::sleep(Duration::from_millis(1200));
        let level = kitchen_volume(sim);
        assert!(ramp.join().unwrap().is_ok());
        level
    })
}

#[test]
fn a_scene_applied_during_a_ramp_keeps_its_level() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let s = surface(&sim);
    set(&s, 25);
    s.save_scene(&Client::Cli, "Low").unwrap();
    set(&s, 10);
    let level = during_a_ramp(&s, &sim, || {
        s.apply_scene(&Client::Cli, "Low").unwrap();
    });
    assert_eq!(level, 25, "the scene's level stands");
    sim.shutdown();
}

#[test]
fn an_undo_during_a_ramp_keeps_the_level_it_puts_back() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let s = surface(&sim);
    set(&s, 10);
    set(&s, 40);
    let level = during_a_ramp(&s, &sim, || {
        // The newest logged action is the set to 40 (the ramp logs when it
        // ends): undo puts the 10 back.
        s.undo(&Client::Cli, false)
            .unwrap()
            .expect("an action to undo");
    });
    assert_eq!(level, 10, "the level the undo put back stands");
    sim.shutdown();
}
