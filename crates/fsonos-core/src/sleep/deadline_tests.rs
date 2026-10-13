//! Deadline accounting plus real S1/S2 pause/restore behavior.

use super::*;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::TransportState;
use std::sync::atomic::{AtomicBool, Ordering};

fn t0() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-07T22:00:00Z")
        .unwrap()
        .to_utc()
}

fn saved(ends: DateTime<Utc>) -> SleepTimer {
    SleepTimer {
        coordinator: PlayerId("RINCON_TEST".into()),
        room: "Test room".into(),
        ends,
        fading: true,
    }
}

#[test]
fn short_late_and_overdue_timers_receive_only_their_remaining_budget() {
    let cases = [(180, 60, 120), (30, 0, 30), (180, 150, 30), (180, 200, 0)];
    for (after, elapsed, remaining) in cases {
        let timers = SleepTimers::new();
        let timer = saved(t0() + TimeDelta::seconds(after));
        assert!(timers.adopt(timer.clone()));
        let before = Instant::now();
        assert_eq!(timers.due(t0() + TimeDelta::seconds(elapsed)).len(), 1);
        let after = Instant::now();
        let deadline = timers.lock()[&timer.coordinator].fade_deadline.unwrap();
        let budget = Duration::from_secs(remaining);
        assert!(deadline >= before + budget);
        assert!(deadline <= after + budget);
    }
}

#[test]
fn an_early_tick_never_claims_a_timer_and_repeated_ticks_never_extend_it() {
    let timers = SleepTimers::new();
    let timer = saved(t0() + TimeDelta::minutes(3));
    assert!(timers.adopt(timer.clone()));
    assert!(timers.due(t0() + TimeDelta::seconds(59)).is_empty());
    assert!(timers.lock()[&timer.coordinator].fade_deadline.is_none());
    assert_eq!(timers.due(t0() + TimeDelta::minutes(1)).len(), 1);
    let deadline = timers.lock()[&timer.coordinator].fade_deadline;
    assert!(timers.due(t0() + TimeDelta::minutes(2)).is_empty());
    assert_eq!(timers.lock()[&timer.coordinator].fade_deadline, deadline);
}

#[test]
fn restored_timers_keep_the_saved_end_without_serializing_a_monotonic_clock() {
    let timer = saved(t0() + TimeDelta::minutes(3));
    let json = serde_json::to_value(&timer).unwrap();
    assert_eq!(json.as_object().unwrap().len(), 4);
    let restored = serde_json::from_value(json).unwrap();
    let timers = SleepTimers::new();
    assert!(timers.adopt(restored));
    assert!(!timers.get(&timer.coordinator).unwrap().fading);
    assert!(timers.lock()[&timer.coordinator].fade_deadline.is_none());
    let before = Instant::now();
    assert_eq!(timers.due(t0() + TimeDelta::seconds(179)).len(), 1);
    let after = Instant::now();
    let deadline = timers.lock()[&timer.coordinator].fade_deadline.unwrap();
    assert!(deadline >= before + Duration::from_secs(1));
    assert!(deadline <= after + Duration::from_secs(1));
}

fn house() -> (SimHandle, SimLan, Vec<HouseholdState>) {
    let sim = SimHousehold::builder()
        .s1([SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1)])
        .s2([SimPlayerSpec::new("Living Room", SimModel::One)])
        .spawn()
        .unwrap();
    let lan = sim.lan();
    let h = ["Kitchen", "Living Room"]
        .iter()
        .map(|room| {
            let mut h = HouseholdState::default();
            h.apply_topology(&get_zone_group_state(&lan, sim.player(room).unwrap().ip).unwrap());
            h
        })
        .collect();
    (sim, lan, h)
}

fn play(lan: &SimLan, h: &[HouseholdState], room: &str) -> PlayerId {
    let player = crate::resolve_room(h, room).unwrap().player.id.clone();
    crate::control::play_uri(
        lan,
        h,
        &player,
        "x-rincon-mp3radio://stream.example.invalid/sleep.mp3",
        "",
    )
    .unwrap();
    crate::control::set_volume(lan, h, &player, 30).unwrap();
    player
}

fn assert_paused_and_restored(
    sim: &SimHandle,
    lan: &SimLan,
    h: &[HouseholdState],
    player: &PlayerId,
    room: &str,
    from: usize,
) {
    let host = ip(h, player).unwrap();
    assert_eq!(
        soap::get_transport_info(lan, host).unwrap().state,
        TransportState::Paused
    );
    assert_eq!(soap::get_volume(lan, host).unwrap(), 30);
    assert_eq!(soap::get_remaining_sleep_timer(lan, host).unwrap(), None);
    let log = sim.soap_log();
    let log = &log[from..];
    let levels: Vec<u8> = log
        .iter()
        .filter(|e| e.room == room && e.action == "SetVolume")
        .map(|e| {
            e.args
                .iter()
                .find(|(key, _)| key == "DesiredVolume")
                .unwrap()
                .1
                .parse()
                .unwrap()
        })
        .collect();
    assert_eq!(levels, [0, 30], "one final fade step, then restoration");
    let writes: Vec<&str> = log
        .iter()
        .filter(|e| e.room == room && matches!(e.action.as_str(), "SetVolume" | "Pause"))
        .map(|e| e.action.as_str())
        .collect();
    assert_eq!(writes, ["SetVolume", "Pause", "SetVolume"]);
}

#[test]
fn overdue_timers_pause_without_a_new_fade_in_both_generations() {
    let (sim, lan, h) = house();
    for room in ["Kitchen", "Living Room"] {
        let player = play(&lan, &h, room);
        let timers = SleepTimers::with_fade(Duration::from_millis(500));
        let fader = Fader::with_timing(Duration::from_millis(50), Duration::from_millis(50));
        timers
            .start(&lan, &h, &fader, &player, Duration::from_secs(1), t0())
            .unwrap();
        assert_eq!(timers.due(t0() + TimeDelta::seconds(2)).len(), 1);
        let from = sim.soap_log().len();
        assert_eq!(
            timers.run(&lan, &h, &fader, &player).unwrap(),
            SleepOutcome::Paused
        );
        assert_paused_and_restored(&sim, &lan, &h, &player, room, from);
        assert!(timers.get(&player).is_none());
    }
}

#[test]
fn a_worker_dispatched_after_its_deadline_does_not_restart_the_fade() {
    let (sim, lan, h) = house();
    for room in ["Kitchen", "Living Room"] {
        let player = play(&lan, &h, room);
        let timers = SleepTimers::with_fade(Duration::from_millis(500));
        let fader = Fader::with_timing(Duration::from_millis(50), Duration::from_millis(50));
        timers
            .start(&lan, &h, &fader, &player, Duration::from_secs(1), t0())
            .unwrap();
        assert_eq!(timers.due(t0() + TimeDelta::milliseconds(500)).len(), 1);
        // Advance the internal deadline instead of making the test scheduler
        // race a short sleep. The public timer's saved end remains untouched.
        timers.lock().get_mut(&player).unwrap().fade_deadline = Some(Instant::now());
        let from = sim.soap_log().len();
        assert_eq!(
            timers.run(&lan, &h, &fader, &player).unwrap(),
            SleepOutcome::Paused
        );
        assert_paused_and_restored(&sim, &lan, &h, &player, room, from);
    }
}

struct SlowRead<'a> {
    lan: &'a SimLan,
    delayed: AtomicBool,
}

impl Transport for SlowRead<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let result = self.lan.soap_post(host, path, action, body);
        if action.trim_matches('"').ends_with("#GetVolume")
            && !self.delayed.swap(true, Ordering::AcqRel)
        {
            std::thread::sleep(Duration::from_millis(150));
        }
        result
    }
}

#[test]
fn snapshot_reads_consume_a_short_timers_budget_in_both_generations() {
    let (sim, lan, h) = house();
    for room in ["Kitchen", "Living Room"] {
        let player = play(&lan, &h, room);
        let timers = SleepTimers::with_fade(Duration::from_millis(500));
        let fader = Fader::with_timing(Duration::from_millis(50), Duration::from_millis(50));
        timers
            .start(&lan, &h, &fader, &player, Duration::from_millis(100), t0())
            .unwrap();
        assert_eq!(timers.due(t0()).len(), 1);
        let from = sim.soap_log().len();
        let slow = SlowRead {
            lan: &lan,
            delayed: AtomicBool::new(false),
        };
        assert_eq!(
            timers.run(&slow, &h, &fader, &player).unwrap(),
            SleepOutcome::Paused
        );
        assert_paused_and_restored(&sim, &lan, &h, &player, room, from);
    }
}
