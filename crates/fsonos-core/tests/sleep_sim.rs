//! The sleep timer against `fsonos-sim` over real loopback sockets: the
//! fade, the pause, the volumes put back, cancel and extend during the fade,
//! and the player's own timer as the backstop.

use chrono::{DateTime, TimeDelta, Utc};
use fsonos_core::fade::Fader;
use fsonos_core::sleep::{SleepOutcome, SleepTimers};
use fsonos_core::{HouseholdState, control, resolve_room};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::net::IpAddr;
use std::time::Duration;

fn houses(sim: &SimHandle, lan: &SimLan) -> Vec<HouseholdState> {
    let mut st = HouseholdState::default();
    st.apply_topology(&get_zone_group_state(lan, sim.player("Kitchen").unwrap().ip).unwrap());
    vec![st]
}

fn ip(h: &[HouseholdState], room: &str) -> IpAddr {
    resolve_room(h, room).unwrap().player.ip
}

fn t0() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-07T22:00:00Z")
        .unwrap()
        .to_utc()
}

fn mins(n: i64) -> TimeDelta {
    TimeDelta::minutes(n)
}

/// Kitchen (with Office joined) plays a queue; Kitchen at 30, Office at 20.
fn playing_kitchen() -> (SimHandle, SimLan, Vec<HouseholdState>, PlayerId) {
    let sim = SimHousehold::builder()
        .s1([
            SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1),
            SimPlayerSpec::new("Office", SimModel::Play5Gen1),
        ])
        .spawn()
        .unwrap();
    let lan = sim.lan();
    let h = houses(&sim, &lan);
    let kitchen = resolve_room(&h, "Kitchen").unwrap().player.id.clone();
    let office = resolve_room(&h, "Office").unwrap().player.id.clone();
    control::queue_spotify_tracks(
        &lan,
        &h,
        &kitchen,
        &[("spotify:track:0SimSleepA00000000001", "Lullaby")],
    )
    .unwrap()
    .unwrap();
    control::play_queue_from(&lan, &h, &kitchen, 1).unwrap();
    control::join(&lan, &h, &office, &kitchen).unwrap();
    control::set_volume(&lan, &h, &kitchen, 30).unwrap();
    control::set_volume(&lan, &h, &office, 20).unwrap();
    let h = houses(&sim, &lan);
    (sim, lan, h, kitchen)
}

fn fader() -> Fader {
    Fader::with_timing(Duration::from_millis(5), Duration::from_millis(5))
}

fn state(lan: &SimLan, h: &[HouseholdState]) -> (TransportState, u8, u8) {
    (
        soap::get_transport_info(lan, ip(h, "Kitchen"))
            .unwrap()
            .state,
        soap::get_volume(lan, ip(h, "Kitchen")).unwrap(),
        soap::get_volume(lan, ip(h, "Office")).unwrap(),
    )
}

#[test]
fn a_sleep_timer_fades_pauses_and_puts_the_volumes_back() {
    let (sim, lan, h, kitchen) = playing_kitchen();
    let fader = fader();
    let timers = SleepTimers::with_fade(Duration::from_millis(200));
    let timer = timers
        .start(&lan, &h, &fader, &kitchen, Duration::from_mins(10), t0())
        .unwrap();
    assert_eq!(
        (timer.room.as_str(), timer.ends),
        ("Kitchen", t0() + mins(10))
    );
    // The player's own timer is set 5 minutes later, as the backstop.
    assert_eq!(
        soap::get_remaining_sleep_timer(&lan, ip(&h, "Kitchen")).unwrap(),
        Some(15 * 60)
    );
    assert!(timers.due(t0() + mins(9)).is_empty());
    assert_eq!(timers.due(t0() + mins(10)).len(), 1);
    let log_from = sim.soap_log().len();
    assert_eq!(
        timers.run(&lan, &h, &fader, &kitchen).unwrap(),
        SleepOutcome::Paused
    );
    // Both rooms went all the way down before the pause.
    let log = &sim.soap_log()[log_from..];
    for room in ["Kitchen", "Office"] {
        assert!(
            log.iter().any(|e| e.room == room
                && e.action == "SetVolume"
                && e.args.iter().any(|(k, v)| k == "DesiredVolume" && v == "0")),
            "{room}"
        );
    }
    assert_eq!(state(&lan, &h), (TransportState::Paused, 30, 20));
    assert_eq!(
        soap::get_remaining_sleep_timer(&lan, ip(&h, "Kitchen")).unwrap(),
        None
    );
    assert_eq!(timers.get(&kitchen), None);
    assert_eq!(
        timers.run(&lan, &h, &fader, &kitchen).unwrap(),
        SleepOutcome::Gone
    );
}

#[test]
fn extending_or_cancelling_during_the_fade_keeps_the_music_at_its_level() {
    let (_sim, lan, h, kitchen) = playing_kitchen();
    let fader = fader();
    let timers = SleepTimers::with_fade(Duration::from_secs(2));
    let pause = Duration::from_millis(300);
    timers
        .start(&lan, &h, &fader, &kitchen, Duration::from_mins(10), t0())
        .unwrap();
    let longer = timers
        .extend(&lan, &h, &fader, &kitchen, Duration::from_mins(15), t0())
        .unwrap()
        .unwrap();
    assert_eq!(longer.ends, t0() + mins(25));
    assert_eq!(
        soap::get_remaining_sleep_timer(&lan, ip(&h, "Kitchen")).unwrap(),
        Some(30 * 60)
    );
    assert!(timers.due(t0() + mins(24)).is_empty());
    assert_eq!(timers.due(t0() + mins(25)).len(), 1);

    // Extended during the fade: the fade stops and the levels come back.
    let outcome = std::thread::scope(|s| {
        let run = s.spawn(|| timers.run(&lan, &h, &fader, &kitchen));
        std::thread::sleep(pause);
        let again = timers
            .extend(
                &lan,
                &h,
                &fader,
                &kitchen,
                Duration::from_mins(5),
                t0() + mins(25),
            )
            .unwrap()
            .unwrap();
        assert_eq!((again.ends, again.fading), (t0() + mins(30), false));
        run.join().unwrap().unwrap()
    });
    assert_eq!(outcome, SleepOutcome::Cancelled);
    assert_eq!(state(&lan, &h), (TransportState::Playing, 30, 20));
    assert_eq!(
        soap::get_remaining_sleep_timer(&lan, ip(&h, "Kitchen")).unwrap(),
        Some(10 * 60)
    );

    // Cancelled during the fade: the same, and no timer left anywhere.
    assert_eq!(timers.due(t0() + mins(30)).len(), 1);
    let outcome = std::thread::scope(|s| {
        let run = s.spawn(|| timers.run(&lan, &h, &fader, &kitchen));
        std::thread::sleep(pause);
        assert!(timers.cancel(&lan, &h, &fader, &kitchen).unwrap());
        run.join().unwrap().unwrap()
    });
    assert_eq!(outcome, SleepOutcome::Cancelled);
    assert_eq!(state(&lan, &h), (TransportState::Playing, 30, 20));
    assert_eq!(timers.get(&kitchen), None);
    assert_eq!(
        soap::get_remaining_sleep_timer(&lan, ip(&h, "Kitchen")).unwrap(),
        None
    );
    assert!(!timers.cancel(&lan, &h, &fader, &kitchen).unwrap());

    // Someone turns the volume up during the fade: the timer is over, and
    // their level stands.
    timers
        .start(&lan, &h, &fader, &kitchen, Duration::from_mins(1), t0())
        .unwrap();
    assert_eq!(timers.due(t0() + mins(1)).len(), 1);
    let outcome = std::thread::scope(|s| {
        let run = s.spawn(|| timers.run(&lan, &h, &fader, &kitchen));
        std::thread::sleep(pause);
        fader.set_volume(&lan, &h, &kitchen, 50).unwrap();
        run.join().unwrap().unwrap()
    });
    assert_eq!(outcome, SleepOutcome::Interrupted);
    let (transport, kitchen_level, _) = state(&lan, &h);
    assert_eq!((transport, kitchen_level), (TransportState::Playing, 50));
    assert_eq!(timers.get(&kitchen), None);
}

#[test]
fn the_players_own_timer_pauses_when_the_daemon_never_fades() {
    let (sim, lan, h, kitchen) = playing_kitchen();
    let timers = SleepTimers::new();
    timers
        .start(&lan, &h, &fader(), &kitchen, Duration::from_mins(10), t0())
        .unwrap();
    sim.clock().advance(Duration::from_secs(15 * 60 - 1));
    assert_eq!(state(&lan, &h).0, TransportState::Playing);
    sim.clock().advance(Duration::from_secs(2));
    assert_eq!(state(&lan, &h).0, TransportState::Paused);
    assert_eq!(
        soap::get_remaining_sleep_timer(&lan, ip(&h, "Kitchen")).unwrap(),
        None
    );
}
