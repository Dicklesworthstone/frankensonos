//! Schedules against `fsonos-sim` with a fake clock, the way the daemon's
//! tick runs them: each run happens once, at its time, by claiming it in the
//! store first; a run missed while the daemon was down is skipped.

use fsonos_core::clock::{Clock, FakeClock};
use fsonos_core::schedule::{self, DEFAULT_GRACE, ScheduleAction, ScheduleSpec};
use fsonos_core::store::{MemStore, Store};
use fsonos_core::{HouseholdState, control, resolve_room};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::TransportState;

/// One daemon tick: claim and carry out every due run. Returns what ran.
fn tick(
    store: &mut MemStore,
    clock: &FakeClock,
    lan: &SimLan,
    h: &[HouseholdState],
) -> Vec<String> {
    let now = clock.now();
    let schedules = schedule::load(store).unwrap();
    let mut ran = Vec::new();
    for (id, due) in schedule::due(&schedules, now.to_utc(), now.offset(), DEFAULT_GRACE) {
        if !schedule::claim(store, id, due).unwrap() {
            continue;
        }
        let s = schedules.iter().find(|s| s.id == id).unwrap();
        let target = |room: &str| resolve_room(h, room).unwrap();
        match &s.action {
            ScheduleAction::Volume { room, level } => {
                control::set_volume(lan, h, &target(room).player.id, *level).unwrap();
            }
            ScheduleAction::Pause { room, .. } => {
                control::pause(lan, h, &target(room).coordinator.id).unwrap();
            }
            other => panic!("not in this test: {other:?}"),
        }
        ran.push(format!("{} by {}", s.spec, s.creator));
    }
    ran
}

#[test]
fn schedules_run_once_at_their_time_and_skip_what_was_missed() {
    let sim = SimHousehold::builder()
        .s1([SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1)])
        .spawn()
        .unwrap();
    let lan = sim.lan();
    let mut h = HouseholdState::default();
    h.apply_topology(&get_zone_group_state(&lan, sim.player("Kitchen").unwrap().ip).unwrap());
    let h = [h];
    let kitchen = resolve_room(&h, "Kitchen").unwrap();
    let (kitchen_id, host) = (kitchen.player.id.clone(), kitchen.player.ip);
    control::play_uri(
        &lan,
        &h,
        &kitchen_id,
        "x-rincon-mp3radio://radio.example/a",
        "",
    )
    .unwrap();

    // Monday 2026-10-05, 07:00 in the house's zone.
    let clock =
        FakeClock::new(chrono::DateTime::parse_from_rfc3339("2026-10-05T07:00:00-04:00").unwrap());
    let mut store = MemStore::default();
    let spec = |s: &str| ScheduleSpec::parse(s, clock.now()).unwrap();
    schedule::add(
        &mut store,
        &spec("weekdays 07:30"),
        &ScheduleAction::Volume {
            room: "Kitchen".into(),
            level: 25,
        },
        "tag:assistant",
        clock.now().to_utc(),
    )
    .unwrap();
    schedule::add(
        &mut store,
        &spec("daily 22:30"),
        &ScheduleAction::Pause {
            room: "Kitchen".into(),
            fade_secs: 0,
        },
        "cli",
        clock.now().to_utc(),
    )
    .unwrap();
    let volume = || soap::get_volume(&lan, host).unwrap();
    let state = || soap::get_transport_info(&lan, host).unwrap().state;

    assert!(tick(&mut store, &clock, &lan, &h).is_empty());
    clock.advance(chrono::TimeDelta::minutes(31));
    assert_eq!(
        tick(&mut store, &clock, &lan, &h),
        ["weekdays 07:30 by tag:assistant"]
    );
    assert_eq!(volume(), 25);
    assert!(tick(&mut store, &clock, &lan, &h).is_empty(), "never twice");

    control::set_volume(&lan, &h, &kitchen_id, 40).unwrap();
    clock.advance(chrono::TimeDelta::hours(15));
    assert_eq!(tick(&mut store, &clock, &lan, &h), ["daily 22:30 by cli"]);
    assert_eq!(state(), TransportState::Paused);

    // Down overnight and well past Tuesday 07:30: that run is skipped.
    clock.set(chrono::DateTime::parse_from_rfc3339("2026-10-06T08:00:00-04:00").unwrap());
    assert!(tick(&mut store, &clock, &lan, &h).is_empty());
    assert_eq!(volume(), 40);
    clock.set(chrono::DateTime::parse_from_rfc3339("2026-10-07T07:35:00-04:00").unwrap());
    assert_eq!(
        tick(&mut store, &clock, &lan, &h),
        ["weekdays 07:30 by tag:assistant"]
    );
    assert_eq!(volume(), 25);
    let fired: Vec<Option<i64>> = store
        .schedules()
        .unwrap()
        .iter()
        .map(|s| s.last_fired)
        .collect();
    assert_eq!(fired.len(), 2);
    assert!(fired.iter().all(Option::is_some));
}
