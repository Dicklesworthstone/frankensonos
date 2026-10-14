//! A sleep timer follows the music when it moves: after `move` the timer
//! (and the player's own one) belongs to the coordinator the music plays
//! under now, not to the room it left. Against `fsonos-sim` players over
//! real loopback sockets.

use fsonos_api::plan::{plan_move, plan_play};
use fsonos_api::surface::schedules::{Sleep, SleepRequest};
use fsonos_api::surface::{Surface, Survey};
use fsonos_api::{MoveRequest, PlayRequest};
use fsonos_core::HouseholdState;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_proto::control::get_remaining_sleep_timer;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHousehold, SimModel, SimPlayerSpec};

#[test]
fn a_sleep_timer_follows_the_music_when_it_moves() {
    let sim = SimHousehold::builder()
        .s1([
            SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1),
            SimPlayerSpec::new("Office", SimModel::Play5Gen1),
        ])
        .spawn()
        .unwrap();
    let lan = sim.lan();
    let kitchen = sim.player("Kitchen").unwrap().ip;
    let office = sim.player("Office").unwrap().ip;
    let survey: Survey = Box::new(move |t| {
        let mut st = HouseholdState::default();
        st.apply_topology(&get_zone_group_state(t, kitchen).map_err(fsonos_core::CoreError::from)?);
        Ok(vec![st])
    });
    let surface = Surface::new(
        Box::new(sim.lan()),
        survey,
        Policy::default(),
        Box::new(SystemClock),
    )
    .with_sleep(Sleep::default());
    let me = Client::LoopbackHttp;
    let play = PlayRequest {
        zone: "Kitchen".into(),
        source_uri: "x-rincon-mp3radio://stream.example.invalid/moving.mp3".into(),
        title: None,
    };
    surface
        .control(&me, "play", |rooms| plan_play(rooms, &play))
        .unwrap();
    let sleep = SleepRequest {
        zone: "Kitchen".into(),
        duration: Some("45m".into()),
        extend: false,
        cancel: false,
    };
    surface.set_sleep_timer(&me, &sleep).unwrap();

    let to_office = MoveRequest {
        zone: "Kitchen".into(),
        to: "Office".into(),
        copy: false,
    };
    surface
        .control(&me, "move_playback", |rooms| plan_move(rooms, &to_office))
        .unwrap();

    let timers = surface.sleep_timers(&me, None).unwrap();
    let running: Vec<_> = timers.iter().filter(|t| t.ends.is_some()).collect();
    assert_eq!(running.len(), 1, "{timers:?}");
    assert_eq!(running[0].zone, "Office", "{timers:?}");
    assert!(get_remaining_sleep_timer(&lan, office).unwrap().is_some());
    assert_eq!(get_remaining_sleep_timer(&lan, kitchen).unwrap(), None);
}
