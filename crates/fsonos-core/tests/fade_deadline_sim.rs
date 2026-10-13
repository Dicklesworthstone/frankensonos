//! Slow real-socket responses consume fade budgets; old steps are not replayed.

use fsonos_core::fade::{FadeOutcome, Fader};
use fsonos_core::{HouseholdState, control, resolve_room};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::PlayerId;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

fn house() -> (SimHandle, SimLan, Vec<HouseholdState>) {
    let sim = SimHousehold::builder()
        .s1([SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1)])
        .s2([SimPlayerSpec::new("Living Room", SimModel::One)])
        .spawn()
        .unwrap();
    let lan = sim.lan();
    let households = ["Kitchen", "Living Room"]
        .iter()
        .map(|room| {
            let mut h = HouseholdState::default();
            h.apply_topology(&get_zone_group_state(&lan, sim.player(room).unwrap().ip).unwrap());
            h
        })
        .collect();
    (sim, lan, households)
}

fn levels(sim: &SimHandle, room: &str) -> Vec<u8> {
    sim.soap_log()
        .iter()
        .filter(|e| e.room == room && e.action == "SetVolume" && e.result.is_ok())
        .map(|e| {
            e.args
                .iter()
                .find(|(key, _)| key == "DesiredVolume")
                .unwrap()
                .1
                .parse()
                .unwrap()
        })
        .collect()
}

struct SlowReply<'a> {
    lan: &'a SimLan,
    action: &'static str,
    delayed: AtomicBool,
}

impl Transport for SlowReply<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let result = self.lan.soap_post(host, path, action, body);
        if action.trim_matches('"').ends_with(self.action)
            && !self.delayed.swap(true, Ordering::AcqRel)
        {
            // Longer than the entire requested fade. Assertions concern the
            // resulting commands, not a tight wall-clock completion window.
            std::thread::sleep(Duration::from_millis(150));
        }
        result
    }
}

#[test]
fn a_slow_state_read_uses_the_fade_budget_in_both_generations() {
    let (sim, lan, h) = house();
    let fader = Fader::with_timing(Duration::from_millis(20), Duration::from_millis(20));
    for room in ["Kitchen", "Living Room"] {
        let player = &resolve_room(&h, room).unwrap().player.id;
        let slow = SlowReply {
            lan: &lan,
            action: "#GetVolume",
            delayed: AtomicBool::new(false),
        };
        assert_eq!(
            fader
                .fade(&slow, &h, player, 80, Duration::from_millis(100))
                .unwrap(),
            FadeOutcome::Reached
        );
        assert_eq!(levels(&sim, room), [80], "no fresh fade after the read");
    }
}

#[test]
fn a_slow_write_skips_obsolete_steps_in_both_generations() {
    let (sim, lan, h) = house();
    let fader = Fader::with_timing(Duration::from_millis(20), Duration::from_millis(20));
    for room in ["Kitchen", "Living Room"] {
        let player = &resolve_room(&h, room).unwrap().player.id;
        let slow = SlowReply {
            lan: &lan,
            action: "#SetVolume",
            delayed: AtomicBool::new(false),
        };
        assert_eq!(
            fader
                .fade(&slow, &h, player, 80, Duration::from_millis(100))
                .unwrap(),
            FadeOutcome::Reached
        );
        let sent = levels(&sim, room);
        assert!(sent.len() <= 2, "first step, then final target: {sent:?}");
        assert_eq!(sent.last(), Some(&80));
        assert!(sent.windows(2).all(|pair| pair[0] < pair[1]));
    }
}

#[test]
fn scene_fades_share_one_budget_across_all_initial_reads() {
    let (sim, lan, h) = house();
    let targets: Vec<_> = [("Kitchen", 40), ("Living Room", 60)]
        .into_iter()
        .map(|(room, to)| (resolve_room(&h, room).unwrap().player.id.clone(), to))
        .collect();
    let slow = SlowReply {
        lan: &lan,
        action: "#GetVolume",
        delayed: AtomicBool::new(false),
    };
    let fader = Fader::with_timing(Duration::from_millis(20), Duration::from_millis(20));
    assert!(
        fader
            .fade_each(&slow, &h, &targets, Duration::from_millis(100))
            .unwrap()
            .is_empty()
    );
    assert_eq!(levels(&sim, "Kitchen"), [40]);
    assert_eq!(levels(&sim, "Living Room"), [60]);
}

struct OverrideDuringRead<'a> {
    lan: &'a SimLan,
    fader: &'a Fader,
    households: &'a [HouseholdState],
    player: &'a PlayerId,
    changed: AtomicBool,
}

impl Transport for OverrideDuringRead<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let result = self.lan.soap_post(host, path, action, body);
        if action.trim_matches('"').ends_with("#GetVolume")
            && !self.changed.swap(true, Ordering::AcqRel)
        {
            self.fader
                .set_volume(self.lan, self.households, self.player, 7)
                .unwrap();
        }
        result
    }
}

#[test]
fn a_new_scene_volume_intent_during_initial_reads_wins() {
    let (sim, lan, h) = house();
    let kitchen = resolve_room(&h, "Kitchen").unwrap().player.id.clone();
    let living = resolve_room(&h, "Living Room").unwrap().player.id.clone();
    let fader = Fader::default();
    let transport = OverrideDuringRead {
        lan: &lan,
        fader: &fader,
        households: &h,
        player: &kitchen,
        changed: AtomicBool::new(false),
    };
    let left = fader
        .fade_each(
            &transport,
            &h,
            &[(kitchen.clone(), 40), (living.clone(), 60)],
            Duration::ZERO,
        )
        .unwrap();
    assert_eq!(left, [kitchen.clone()]);
    assert_eq!(control::volume(&lan, &h, &kitchen).unwrap(), 7);
    assert_eq!(control::volume(&lan, &h, &living).unwrap(), 60);
    assert_eq!(
        levels(&sim, "Kitchen"),
        [7],
        "no stale scene volume after the override"
    );
}

#[test]
fn an_expired_absolute_deadline_sends_only_the_target() {
    let (sim, lan, h) = house();
    let fader = Fader::default();
    for room in ["Kitchen", "Living Room"] {
        let player = &resolve_room(&h, room).unwrap().player.id;
        let claimed = fader.claim(std::slice::from_ref(player));
        let deadline = Instant::now();
        assert_eq!(
            fader
                .fade_group_claimed_until(&lan, &h, &claimed, 0, deadline)
                .unwrap(),
            FadeOutcome::Reached
        );
        assert_eq!(levels(&sim, room), [0]);
    }
}
