//! The sleep timer against `fsonos-sim` over real loopback sockets: the
//! fade, the pause, the volumes put back, cancel and extend during the fade,
//! and the player's own timer as the backstop.

use chrono::{DateTime, TimeDelta, Utc};
use fsonos_core::fade::Fader;
use fsonos_core::sleep::{SleepOutcome, SleepTimers};
use fsonos_core::{HouseholdState, control, resolve_room};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
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
    assert_eq!(
        timers
            .due(t0() + mins(10) - TimeDelta::milliseconds(200))
            .len(),
        1
    );
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

/// The simulator's LAN, except that `action` sent to `host` fails as if the
/// player had dropped off the network.
struct Failing<'a> {
    lan: &'a SimLan,
    action: &'static str,
    host: IpAddr,
}

impl Transport for Failing<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        control_path: &str,
        soap_action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if host == self.host
            && soap_action
                .trim_matches('"')
                .ends_with(&format!("#{}", self.action))
        {
            return Err(ProtoError::Network {
                target: host.to_string(),
                detail: "unreachable".into(),
            });
        }
        self.lan.soap_post(host, control_path, soap_action, body)
    }
}

#[test]
fn a_fade_that_fails_puts_the_volumes_back_and_lets_go_of_the_timer() {
    let (_sim, lan, h, kitchen) = playing_kitchen();
    let fader = fader();
    let timers = SleepTimers::with_fade(Duration::from_millis(200));
    timers
        .start(&lan, &h, &fader, &kitchen, Duration::from_mins(10), t0())
        .unwrap();
    assert_eq!(timers.due(t0() + mins(10)).len(), 1);
    // The Office drops off mid-fade: its volume can no longer be set.
    let failing = Failing {
        lan: &lan,
        action: "SetVolume",
        host: ip(&h, "Office"),
    };
    assert!(timers.run(&failing, &h, &fader, &kitchen).is_err());
    assert_eq!(timers.get(&kitchen), None, "not left fading");
    // The Kitchen is back at its level, and the player's own timer is still
    // set to pause the group.
    assert_eq!(state(&lan, &h), (TransportState::Playing, 30, 20));
    assert!(
        soap::get_remaining_sleep_timer(&lan, ip(&h, "Kitchen"))
            .unwrap()
            .is_some()
    );
}

#[test]
fn a_pause_that_fails_still_puts_the_volumes_back() {
    let (_sim, lan, h, kitchen) = playing_kitchen();
    let fader = fader();
    let timers = SleepTimers::with_fade(Duration::from_millis(200));
    timers
        .start(&lan, &h, &fader, &kitchen, Duration::from_mins(10), t0())
        .unwrap();
    assert_eq!(timers.due(t0() + mins(10)).len(), 1);
    let failing = Failing {
        lan: &lan,
        action: "Pause",
        host: ip(&h, "Kitchen"),
    };
    assert!(timers.run(&failing, &h, &fader, &kitchen).is_err());
    assert_eq!(timers.get(&kitchen), None, "not left fading");
    // Not silent for its next play: the levels are back, and the player's own
    // timer pauses the group.
    assert_eq!(state(&lan, &h), (TransportState::Playing, 30, 20));
    assert!(
        soap::get_remaining_sleep_timer(&lan, ip(&h, "Kitchen"))
            .unwrap()
            .is_some()
    );
}

#[test]
fn a_timer_whose_group_is_gone_is_let_go() {
    let (_sim, lan, h, kitchen) = playing_kitchen();
    let fader = fader();
    let timers = SleepTimers::with_fade(Duration::from_millis(200));
    timers
        .start(&lan, &h, &fader, &kitchen, Duration::from_mins(10), t0())
        .unwrap();
    assert_eq!(timers.due(t0() + mins(10)).len(), 1);
    // The house can't be read just now: the run fails and lets the timer go.
    assert!(timers.run(&lan, &[], &fader, &kitchen).is_err());
    assert_eq!(timers.get(&kitchen), None, "not left fading");
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
    assert_eq!(
        timers.due(t0() + mins(25) - TimeDelta::seconds(2)).len(),
        1
    );

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
    assert_eq!(
        timers.due(t0() + mins(30) - TimeDelta::seconds(2)).len(),
        1
    );
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
    assert_eq!(
        timers.due(t0() + mins(1) - TimeDelta::seconds(2)).len(),
        1
    );
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
fn interrupt_ends_a_fade_in_flight_without_putting_volumes_back() {
    // The product volume path (control::set_volume) supersedes the fade through
    // `interrupt`, which sets no volume of its own (unlike `Fader::set_volume`):
    // the fade stops, the group plays on at its faded level, and the timer is over.
    let (_sim, lan, h, kitchen) = playing_kitchen();
    let fader = fader();
    let timers = SleepTimers::with_fade(Duration::from_secs(2));
    let pause = Duration::from_millis(300);
    timers
        .start(&lan, &h, &fader, &kitchen, Duration::from_mins(1), t0())
        .unwrap();
    assert_eq!(
        timers.due(t0() + mins(1) - TimeDelta::seconds(2)).len(),
        1
    );
    let (outcome, superseded) = std::thread::scope(|s| {
        let run = s.spawn(|| timers.run(&lan, &h, &fader, &kitchen));
        std::thread::sleep(pause);
        let superseded = timers.interrupt(&fader, &kitchen);
        (run.join().unwrap().unwrap(), superseded)
    });
    assert!(superseded, "a fade was in flight to supersede");
    assert_eq!(outcome, SleepOutcome::Interrupted);
    assert_eq!(state(&lan, &h).0, TransportState::Playing);
    assert_eq!(timers.get(&kitchen), None);
    // With no fade in flight, interrupt does nothing.
    assert!(!timers.interrupt(&fader, &kitchen));
}

#[test]
fn the_players_own_timer_pauses_when_the_daemon_never_fades() {
    let (sim, lan, h, kitchen) = playing_kitchen();
    // A stream, which (unlike the queue) plays on for the whole quarter hour.
    control::play_uri(
        &lan,
        &h,
        &kitchen,
        "x-rincon-mp3radio://radio.example/night",
        "",
    )
    .unwrap();
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

/// Kitchen and Office each on their own, the Kitchen playing a queue at 30,
/// the Office a stream at 20.
fn two_groups() -> (SimHandle, SimLan, Vec<HouseholdState>, PlayerId, PlayerId) {
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
        &[("spotify:track:0SimSleepB00000000001", "Nocturne")],
    )
    .unwrap()
    .unwrap();
    control::play_queue_from(&lan, &h, &kitchen, 1).unwrap();
    control::play_uri(
        &lan,
        &h,
        &office,
        "x-rincon-mp3radio://stream.example.invalid/office.mp3",
        "",
    )
    .unwrap();
    control::set_volume(&lan, &h, &kitchen, 30).unwrap();
    control::set_volume(&lan, &h, &office, 20).unwrap();
    (sim, lan, h, kitchen, office)
}

#[test]
fn a_timer_follows_the_music_to_its_new_coordinator() {
    let (_sim, lan, h, kitchen, office) = two_groups();
    let fader = fader();
    let timers = SleepTimers::with_fade(Duration::from_millis(200));
    timers
        .start(&lan, &h, &fader, &kitchen, Duration::from_mins(45), t0())
        .unwrap();
    // The music moved from the Kitchen to the Office.
    assert!(
        timers
            .rekey(&lan, &h, &fader, &kitchen, &office, t0() + mins(5))
            .unwrap()
    );
    assert_eq!(timers.get(&kitchen), None);
    let moved = timers.get(&office).expect("the Office's timer now");
    assert_eq!(
        (moved.room.as_str(), moved.ends),
        ("Office", t0() + mins(45))
    );
    // The player's own timer moved too: the Kitchen will not pause later.
    assert!(
        soap::get_remaining_sleep_timer(&lan, ip(&h, "Office"))
            .unwrap()
            .is_some()
    );
    assert_eq!(
        soap::get_remaining_sleep_timer(&lan, ip(&h, "Kitchen")).unwrap(),
        None
    );
    // Nothing to move from a group without a timer.
    assert!(
        !timers
            .rekey(&lan, &h, &fader, &kitchen, &office, t0())
            .unwrap()
    );
}

#[test]
fn a_timer_stays_put_when_the_new_group_cannot_take_it() {
    let (_sim, lan, h, kitchen, office) = two_groups();
    let fader = fader();
    let timers = SleepTimers::with_fade(Duration::from_millis(200));
    timers
        .start(&lan, &h, &fader, &kitchen, Duration::from_mins(45), t0())
        .unwrap();
    let failing = Failing {
        lan: &lan,
        action: "ConfigureSleepTimer",
        host: ip(&h, "Office"),
    };
    assert!(
        timers
            .rekey(&failing, &h, &fader, &kitchen, &office, t0())
            .is_err()
    );
    assert!(timers.get(&kitchen).is_some());
    assert_eq!(timers.get(&office), None);
    assert!(
        soap::get_remaining_sleep_timer(&lan, ip(&h, "Kitchen"))
            .unwrap()
            .is_some(),
        "the Kitchen still pauses on its own"
    );
}

#[test]
fn a_fade_a_move_stopped_starts_over_in_the_new_group_a_moment_later() {
    let (_sim, lan, h, kitchen, office) = two_groups();
    let fader = fader();
    let timers = SleepTimers::with_fade(Duration::from_millis(200));
    timers
        .start(&lan, &h, &fader, &kitchen, Duration::from_mins(10), t0())
        .unwrap();
    let end = t0() + mins(10);
    assert_eq!(timers.due(end).len(), 1, "fading");
    assert!(
        timers
            .rekey(&lan, &h, &fader, &kitchen, &office, end)
            .unwrap()
    );
    // Not due at once: the stopped run puts the volumes back first.
    assert!(timers.due(end).is_empty());
    assert_eq!(timers.due(end + TimeDelta::seconds(3)).len(), 1);
}

#[test]
fn a_room_whose_group_joined_another_stops_alone() {
    let (sim, lan, h, kitchen, office) = two_groups();
    let fader = fader();
    let timers = SleepTimers::with_fade(Duration::from_millis(200));
    timers
        .start(&lan, &h, &fader, &kitchen, Duration::from_mins(10), t0())
        .unwrap();
    // The Kitchen has since joined the Office's group.
    control::join(&lan, &h, &kitchen, &office).unwrap();
    let h = houses(&sim, &lan);
    assert_eq!(timers.due(t0() + mins(10)).len(), 1);
    assert_eq!(
        timers.run(&lan, &h, &fader, &kitchen).unwrap(),
        SleepOutcome::Paused
    );
    // The Kitchen left the group, its level back; the Office plays on,
    // never turned down.
    let groups = get_zone_group_state(&lan, ip(&h, "Office")).unwrap().groups;
    assert!(
        groups
            .iter()
            .any(|g| g.coordinator == kitchen && g.members.iter().all(|m| m.uuid == kitchen)),
        "{groups:?}"
    );
    assert_eq!(
        soap::get_transport_info(&lan, ip(&h, "Office"))
            .unwrap()
            .state,
        TransportState::Playing
    );
    assert_eq!(soap::get_volume(&lan, ip(&h, "Kitchen")).unwrap(), 30);
    assert_eq!(soap::get_volume(&lan, ip(&h, "Office")).unwrap(), 20);
    assert_eq!(timers.get(&kitchen), None);
}

/// The simulator's LAN, except that the first GetVolume waits until the test
/// opens the gate: it holds a sleep run just after it took up its timer.
struct Gated<'a> {
    lan: &'a SimLan,
    reached: AtomicBool,
    open: (Mutex<bool>, Condvar),
}

impl Gated<'_> {
    fn open(&self) {
        *self.open.0.lock().unwrap() = true;
        self.open.1.notify_all();
    }
}

impl Transport for Gated<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        control_path: &str,
        soap_action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if soap_action.trim_matches('"').ends_with("#GetVolume")
            && !self.reached.swap(true, Ordering::AcqRel)
        {
            let mut open = self.open.0.lock().unwrap();
            while !*open {
                open = self.open.1.wait(open).unwrap();
            }
        }
        self.lan.soap_post(host, control_path, soap_action, body)
    }
}

#[test]
fn a_cancel_just_as_the_fade_begins_stops_it_before_any_step() {
    let (sim, lan, h, kitchen) = playing_kitchen();
    let fader = fader();
    let timers = SleepTimers::with_fade(Duration::from_millis(200));
    timers
        .start(&lan, &h, &fader, &kitchen, Duration::from_mins(10), t0())
        .unwrap();
    assert_eq!(timers.due(t0() + mins(10)).len(), 1);
    let gated = Gated {
        lan: &lan,
        reached: AtomicBool::new(false),
        open: (Mutex::new(false), Condvar::new()),
    };
    let steps_before = sim.soap_log().len();
    let outcome = std::thread::scope(|s| {
        let run = s.spawn(|| timers.run(&gated, &h, &fader, &kitchen));
        // The run has taken the timer up and not yet stepped: cancel now.
        while !gated.reached.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(timers.cancel(&lan, &h, &fader, &kitchen).unwrap());
        gated.open();
        run.join().unwrap().unwrap()
    });
    assert_eq!(outcome, SleepOutcome::Cancelled);
    // Not one step down: the only volumes set are the levels put back.
    let set: Vec<String> = sim.soap_log()[steps_before..]
        .iter()
        .filter(|e| e.action == "SetVolume")
        .filter_map(|e| {
            e.args
                .iter()
                .find(|(k, _)| k == "DesiredVolume")
                .map(|(_, v)| v.clone())
        })
        .collect();
    assert!(set.iter().all(|v| v == "30" || v == "20"), "{set:?}");
    assert_eq!(state(&lan, &h), (TransportState::Playing, 30, 20));
}

#[test]
fn a_volume_change_before_the_run_gets_going_ends_the_timer() {
    let (sim, lan, h, kitchen) = playing_kitchen();
    let fader = fader();
    let timers = SleepTimers::with_fade(Duration::from_millis(200));
    timers
        .start(&lan, &h, &fader, &kitchen, Duration::from_mins(10), t0())
        .unwrap();
    assert_eq!(timers.due(t0() + mins(10)).len(), 1);
    // Someone sets a volume after the timer came due, before its run has
    // taken up its rooms (it was still reading the house).
    assert!(timers.interrupt(&fader, &kitchen));
    let from = sim.soap_log().len();
    assert_eq!(
        timers.run(&lan, &h, &fader, &kitchen).unwrap(),
        SleepOutcome::Interrupted
    );
    assert_eq!(timers.get(&kitchen), None);
    // No fade, no pause: the music plays on at its level, and the player's
    // own timer is cleared too.
    let sent: Vec<String> = sim.soap_log()[from..]
        .iter()
        .map(|e| e.action.clone())
        .collect();
    assert!(
        !sent.iter().any(|a| a == "SetVolume" || a == "Pause"),
        "{sent:?}"
    );
    assert_eq!(state(&lan, &h), (TransportState::Playing, 30, 20));
    assert_eq!(
        soap::get_remaining_sleep_timer(&lan, ip(&h, "Kitchen")).unwrap(),
        None
    );
}
