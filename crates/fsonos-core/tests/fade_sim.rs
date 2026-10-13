//! Volume fades against `fsonos-sim` over real loopback sockets, in both
//! generations: stepped fades, supersession by a newer intent, device-speed
//! ramps and their stepped fallback, and fade-out-then-pause.

use fsonos_core::fade::{FadeOutcome, Fader, RampOutcome};
use fsonos_core::{HouseholdState, control, grouping, resolve_room};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::sync::Arc;
use std::time::Duration;

const MS: fn(u64) -> Duration = Duration::from_millis;

fn sim() -> SimHandle {
    SimHousehold::builder()
        .s1([
            SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1),
            SimPlayerSpec::new("Office", SimModel::Play5Gen1),
        ])
        .s2([SimPlayerSpec::new("Living Room", SimModel::One)])
        .spawn()
        .unwrap()
}

fn houses(sim: &SimHandle, lan: &SimLan) -> Vec<HouseholdState> {
    ["Kitchen", "Living Room"]
        .iter()
        .map(|room| {
            let mut st = HouseholdState::default();
            st.apply_topology(&get_zone_group_state(lan, sim.player(room).unwrap().ip).unwrap());
            st
        })
        .collect()
}

fn id(houses: &[HouseholdState], room: &str) -> PlayerId {
    resolve_room(houses, room).unwrap().player.id.clone()
}

fn set_volumes(sim: &SimHandle, room: &str) -> Vec<u8> {
    sim.soap_log()
        .iter()
        .filter(|e| e.room == room && e.action == "SetVolume" && e.result.is_ok())
        .map(|e| {
            e.args
                .iter()
                .find(|(k, _)| k == "DesiredVolume")
                .unwrap()
                .1
                .parse()
                .unwrap()
        })
        .collect()
}

#[test]
fn a_fade_steps_to_its_target_in_both_generations() {
    let sim = sim();
    let lan = sim.lan();
    let h = houses(&sim, &lan);
    let fader = Fader::with_timing(MS(20), MS(20));
    for room in ["Kitchen", "Living Room"] {
        let p = id(&h, room);
        assert_eq!(
            fader.fade(&lan, &h, &p, 80, MS(80)).unwrap(),
            FadeOutcome::Reached
        );
        assert_eq!(control::volume(&lan, &h, &p).unwrap(), 80);
        // The pure clock tests prove each step. Real sockets may skip a
        // deadline under load, but never replay old steps or miss the target.
        let sent = set_volumes(&sim, room);
        assert_eq!(sent.last(), Some(&80));
        assert!(sent.iter().all(|level| [35, 50, 65, 80].contains(level)));
        assert!(sent.windows(2).all(|pair| pair[0] < pair[1]));
    }
}

#[test]
fn a_newer_intent_stops_a_running_fade() {
    let sim = sim();
    let lan = sim.lan();
    let h = houses(&sim, &lan);
    let kitchen = id(&h, "Kitchen");
    let fader = Arc::new(Fader::with_timing(MS(100), MS(100)));
    let running = {
        let (fader, lan, h, kitchen) =
            (Arc::clone(&fader), lan.clone(), h.clone(), kitchen.clone());
        std::thread::spawn(move || fader.fade(&lan, &h, &kitchen, 80, MS(2000)).unwrap())
    };
    std::thread::sleep(MS(350));
    fader.set_volume(&lan, &h, &kitchen, 10).unwrap();
    let outcome = running.join().unwrap();
    let FadeOutcome::Superseded { at } = outcome else {
        panic!("expected the fade to be superseded, got {outcome:?}");
    };
    assert!(at > 20 && at < 80, "stopped part-way, at {at}");
    // No step lands after the newer intent.
    std::thread::sleep(MS(300));
    assert_eq!(control::volume(&lan, &h, &kitchen).unwrap(), 10);
    assert_eq!(*set_volumes(&sim, "Kitchen").last().unwrap(), 10);
}

#[test]
fn device_ramps_and_their_stepped_fallback() {
    let sim = sim();
    let lan = sim.lan();
    let h = houses(&sim, &lan);
    let fader = Fader::with_timing(MS(20), MS(10));

    // A player that ramps itself.
    let kitchen = id(&h, "Kitchen");
    assert!(matches!(
        fader.ramp_at_device_speed(&lan, &h, &kitchen, 50).unwrap(),
        RampOutcome::Device { .. }
    ));
    assert_eq!(control::volume(&lan, &h, &kitchen).unwrap(), 50);

    // Players that refuse RampToVolume, in both generations: the ramp is
    // stepped instead, and the refusal is remembered.
    for room in ["Office", "Living Room"] {
        sim.upnp_fault(room, "RampToVolume", 402).unwrap();
        let p = id(&h, room);
        for target in [40, 25] {
            let outcome = fader.ramp_at_device_speed(&lan, &h, &p, target).unwrap();
            assert_eq!(outcome, RampOutcome::Stepped(FadeOutcome::Reached));
            assert_eq!(control::volume(&lan, &h, &p).unwrap(), target);
        }
        let ramps = sim
            .soap_log()
            .iter()
            .filter(|e| e.room == room && e.action == "RampToVolume")
            .count();
        assert_eq!(ramps, 1, "{room}: RampToVolume is not retried");
    }
}

#[test]
fn fade_out_then_pause_puts_the_volumes_back() {
    let sim = sim();
    let lan = sim.lan();
    let h = houses(&sim, &lan);
    let kitchen = resolve_room(&h, "Kitchen").unwrap();
    let office = resolve_room(&h, "Office").unwrap();
    assert!(grouping::group(&lan, &h, &kitchen, &[office]).is_complete());
    let h = houses(&sim, &lan);
    let coord = id(&h, "Kitchen");
    control::play_uri(
        &lan,
        &h,
        &coord,
        "x-rincon-mp3radio://stream.example.invalid/a.mp3",
        "",
    )
    .unwrap();
    control::set_volume(&lan, &h, &coord, 30).unwrap();
    control::set_volume(&lan, &h, &id(&h, "Office"), 40).unwrap();

    let fader = Fader::with_timing(MS(20), MS(20));
    assert_eq!(
        fader.fade_out_and_pause(&lan, &h, &coord, MS(80)).unwrap(),
        FadeOutcome::Reached
    );

    assert_eq!(
        control::playback(&lan, &h, &coord).unwrap().transport.state,
        TransportState::Paused
    );
    assert_eq!(control::volume(&lan, &h, &coord).unwrap(), 30);
    assert_eq!(control::volume(&lan, &h, &id(&h, "Office")).unwrap(), 40);
    // Both rooms went all the way down before the pause.
    assert!(set_volumes(&sim, "Kitchen").contains(&0));
    assert!(set_volumes(&sim, "Office").contains(&0));
    let log = sim.soap_log();
    let pause = log.iter().position(|e| e.action == "Pause").unwrap();
    let last_zero = log
        .iter()
        .rposition(|e| {
            e.action == "SetVolume" && e.args.iter().any(|(k, v)| k == "DesiredVolume" && v == "0")
        })
        .unwrap();
    assert!(last_zero < pause, "silence first, then pause");
}

/// Kitchen (with Office joined) playing, Kitchen at 30, Office at 40.
fn playing_group(sim: &SimHandle, lan: &SimLan) -> (Vec<HouseholdState>, PlayerId) {
    let h = houses(sim, lan);
    let kitchen = resolve_room(&h, "Kitchen").unwrap();
    let office = resolve_room(&h, "Office").unwrap();
    assert!(grouping::group(lan, &h, &kitchen, &[office]).is_complete());
    let h = houses(sim, lan);
    let coord = id(&h, "Kitchen");
    control::play_uri(
        lan,
        &h,
        &coord,
        "x-rincon-mp3radio://stream.example.invalid/a.mp3",
        "",
    )
    .unwrap();
    control::set_volume(lan, &h, &coord, 30).unwrap();
    control::set_volume(lan, &h, &id(&h, "Office"), 40).unwrap();
    (h, coord)
}

/// Wait until `room` has had a fade step below `level`.
fn wait_for_a_step(sim: &SimHandle, room: &str, level: u8) {
    for _ in 0..1000 {
        if set_volumes(sim, room).iter().any(|v| *v < level) {
            return;
        }
        std::thread::sleep(MS(10));
    }
    panic!("{room}'s fade never started");
}

#[test]
fn a_stopping_daemon_finishes_a_fade_out_at_once_and_puts_the_volumes_back() {
    let sim = sim();
    let lan = sim.lan();
    let (h, coord) = playing_group(&sim, &lan);
    let fader = Fader::with_timing(MS(50), MS(50));
    std::thread::scope(|s| {
        // A minute-long fade-out, stopped by the daemon a few steps in.
        let fading = s.spawn(|| fader.fade_out_and_pause(&lan, &h, &coord, MS(60_000)));
        wait_for_a_step(&sim, "Kitchen", 30);
        let stopping = std::time::Instant::now();
        assert!(fader.finish_all(MS(5000)), "the fade ended in time");
        assert!(stopping.elapsed() < MS(3000), "at once, not a minute later");
        assert_eq!(fading.join().unwrap().unwrap(), FadeOutcome::Reached);
    });
    assert_eq!(
        control::playback(&lan, &h, &coord).unwrap().transport.state,
        TransportState::Paused
    );
    assert_eq!(control::volume(&lan, &h, &coord).unwrap(), 30);
    assert_eq!(control::volume(&lan, &h, &id(&h, "Office")).unwrap(), 40);
}

#[test]
fn a_stopping_daemon_takes_a_ramp_to_its_level_at_once() {
    let sim = sim();
    let lan = sim.lan();
    let h = houses(&sim, &lan);
    let kitchen = id(&h, "Kitchen");
    control::set_volume(&lan, &h, &kitchen, 60).unwrap();
    let fader = Fader::with_timing(MS(50), MS(50));
    std::thread::scope(|s| {
        let ramping = s.spawn(|| fader.fade(&lan, &h, &kitchen, 10, MS(60_000)));
        wait_for_a_step(&sim, "Kitchen", 60);
        assert!(fader.finish_all(MS(5000)));
        assert_eq!(ramping.join().unwrap().unwrap(), FadeOutcome::Reached);
    });
    assert_eq!(control::volume(&lan, &h, &kitchen).unwrap(), 10);
    // A fade asked for after the daemon began stopping finishes at once too.
    let started = std::time::Instant::now();
    assert_eq!(
        fader.fade(&lan, &h, &kitchen, 25, MS(60_000)).unwrap(),
        FadeOutcome::Reached
    );
    assert!(started.elapsed() < MS(3000));
    assert_eq!(control::volume(&lan, &h, &kitchen).unwrap(), 25);
}
