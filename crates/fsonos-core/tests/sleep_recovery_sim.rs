//! Interrupted sleep fades retain their original levels across a fresh runtime.
//! The transport can lose execution before or after a real simulator mutation.

use chrono::{DateTime, Utc};
use fsonos_core::fade::Fader;
use fsonos_core::sleep::{SleepCheckpoint, SleepFade, SleepOutcome, SleepRecovery, SleepTimers};
use fsonos_core::{CoreError, HouseholdState, control, resolve_room};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::net::IpAddr;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

struct House {
    sim: SimHandle,
    lan: SimLan,
    households: Vec<HouseholdState>,
    coordinator: PlayerId,
}

impl House {
    fn new() -> Self {
        let sim = SimHousehold::builder()
            .s2([
                SimPlayerSpec::new("SleepA", SimModel::One),
                SimPlayerSpec::new("SleepB", SimModel::One),
            ])
            .spawn()
            .unwrap();
        let lan = sim.lan();
        let mut household = HouseholdState::default();
        household
            .apply_topology(&get_zone_group_state(&lan, sim.player("SleepA").unwrap().ip).unwrap());
        let households = vec![household];
        let coordinator = resolve_room(&households, "SleepA")
            .unwrap()
            .player
            .id
            .clone();
        let member = resolve_room(&households, "SleepB")
            .unwrap()
            .player
            .id
            .clone();
        control::queue_spotify_tracks(
            &lan,
            &households,
            &coordinator,
            &[("spotify:track:0SimSleepRecovery001", "Night")],
        )
        .unwrap()
        .unwrap();
        control::play_queue_from(&lan, &households, &coordinator, 1).unwrap();
        control::join(&lan, &households, &member, &coordinator).unwrap();
        let mut h = Self {
            sim,
            lan,
            households,
            coordinator,
        };
        h.refresh();
        soap::set_volume(&h.lan, h.ip("SleepA"), 30).unwrap();
        soap::set_volume(&h.lan, h.ip("SleepB"), 20).unwrap();
        h
    }

    fn refresh(&mut self) {
        let topology = get_zone_group_state(&self.lan, self.ip("SleepA")).unwrap();
        self.households[0].apply_topology(&topology);
    }

    fn ip(&self, room: &str) -> IpAddr {
        self.sim.player(room).unwrap().ip
    }

    fn levels(&self) -> (u8, u8) {
        (
            soap::get_volume(&self.lan, self.ip("SleepA")).unwrap(),
            soap::get_volume(&self.lan, self.ip("SleepB")).unwrap(),
        )
    }

    fn state(&self) -> TransportState {
        soap::get_transport_info(&self.lan, self.ip("SleepA"))
            .unwrap()
            .state
    }

    fn crash(
        &self,
        journal: &Journal,
        action: &'static str,
        writes: usize,
        after: bool,
    ) -> SleepFade {
        let timers = SleepTimers::with_fade(Duration::ZERO);
        let fader = Fader::default();
        timers
            .start(
                &self.lan,
                &self.households,
                &fader,
                &self.coordinator,
                Duration::from_secs(1),
                now(),
            )
            .unwrap();
        let ticket = timers
            .due_runs(now() + chrono::TimeDelta::seconds(1), 1)
            .remove(0);
        let crashing = Crash {
            lan: &self.lan,
            action,
            remaining: AtomicUsize::new(writes),
            after,
        };
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                timers
                    .run_due_with_checkpoint(&crashing, &self.households, &fader, &ticket, journal)
                    .unwrap();
            }))
            .is_err()
        );
        journal
            .saved
            .lock()
            .unwrap()
            .clone()
            .expect("journal survived the worker")
    }
}

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-07T22:00:00Z")
        .unwrap()
        .to_utc()
}

#[derive(Default)]
struct Journal {
    saved: Mutex<Option<SleepFade>>,
    fail: bool,
}

impl SleepCheckpoint for Journal {
    fn save(&self, fade: &SleepFade) -> Result<(), CoreError> {
        if self.fail {
            return Err(CoreError::Store("synthetic full disk".into()));
        }
        *self.saved.lock().unwrap() = Some(fade.clone());
        Ok(())
    }
    fn complete(&self, _: &SleepFade) -> Result<(), CoreError> {
        *self.saved.lock().unwrap() = None;
        Ok(())
    }
    fn current(&self, fade: &SleepFade) -> bool {
        self.saved
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|saved| saved.id == fade.id)
    }
}

struct Crash<'a> {
    lan: &'a SimLan,
    action: &'static str,
    remaining: AtomicUsize,
    after: bool,
}

impl Transport for Crash<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        header: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let crash =
            header.contains(self.action) && self.remaining.fetch_sub(1, Ordering::SeqCst) == 1;
        assert!(!crash || self.after, "crash before speaker write");
        let reply = self.lan.soap_post(host, path, header, body)?;
        assert!(!crash, "crash after speaker write, before acknowledgement");
        Ok(reply)
    }
}

#[test]
fn restarting_mid_fade_uses_original_levels_for_both_possible_write_outcomes() {
    for after in [false, true] {
        let h = House::new();
        let journal = Journal::default();
        let fade = h.crash(&journal, "#SetVolume", 2, after);
        assert!(fade.valid());
        assert_eq!(
            fade.recovery(&h.lan, &h.households, now(), true).unwrap(),
            SleepRecovery::Resume
        );
        let timers = SleepTimers::with_fade(Duration::ZERO);
        assert!(timers.adopt_resuming(fade.timer.clone(), fade));
        let ticket = timers
            .due_runs(now() + chrono::TimeDelta::seconds(1), 1)
            .remove(0);
        assert_eq!(
            timers
                .run_due_with_checkpoint(
                    &h.lan,
                    &h.households,
                    &Fader::default(),
                    &ticket,
                    &journal
                )
                .unwrap(),
            SleepOutcome::Paused
        );
        assert_eq!(h.levels(), (30, 20));
        assert_eq!(h.state(), TransportState::Paused);
        assert!(journal.saved.lock().unwrap().is_none());
        assert_eq!(
            h.sim
                .soap_log()
                .iter()
                .filter(|e| e.action == "Pause")
                .count(),
            1
        );
    }
}

#[test]
fn restart_after_an_accepted_pause_only_restores_volume() {
    let h = House::new();
    let journal = Journal::default();
    let mut fade = h.crash(&journal, "#Pause", 1, true);
    assert_eq!(h.levels(), (0, 0));
    assert_eq!(
        fade.recovery(&h.lan, &h.households, now(), true).unwrap(),
        SleepRecovery::Restore
    );
    let before = h.sim.soap_log().len();
    fade.restore(&h.lan, &h.households, &Fader::default(), &journal)
        .unwrap();
    assert_eq!(h.levels(), (30, 20));
    assert_eq!(h.state(), TransportState::Paused);
    assert!(
        !h.sim.soap_log()[before..]
            .iter()
            .any(|e| matches!(e.action.as_str(), "Pause" | "Play" | "SetAVTransportURI"))
    );
}

#[test]
fn a_manual_level_wins_while_other_owned_levels_can_be_cleaned_up() {
    let h = House::new();
    let journal = Journal::default();
    let mut fade = h.crash(&journal, "#SetVolume", 2, true);
    soap::set_volume(&h.lan, h.ip("SleepB"), 44).unwrap();
    assert_eq!(
        fade.recovery(&h.lan, &h.households, now(), true).unwrap(),
        SleepRecovery::Restore
    );
    let before = h.sim.soap_log().len();
    fade.restore(&h.lan, &h.households, &Fader::default(), &journal)
        .unwrap();
    assert_eq!(h.levels(), (30, 44));
    assert_eq!(h.state(), TransportState::Playing);
    assert!(
        !h.sim.soap_log()[before..]
            .iter()
            .any(|e| e.action == "Pause")
    );
}

#[test]
fn a_changed_source_or_group_is_definitively_retired_without_a_volume_write() {
    for regroup in [false, true] {
        let mut h = House::new();
        let journal = Journal::default();
        let mut fade = h.crash(&journal, "#SetVolume", 2, true);
        if regroup {
            control::leave(&h.lan, &h.households, &h.coordinator).unwrap();
            h.refresh();
        } else {
            soap::set_av_transport_uri(
                &h.lan,
                h.ip("SleepA"),
                "https://example.invalid/new-stream",
                "",
            )
            .unwrap();
        }
        let before = h.sim.soap_log().len();
        assert_eq!(
            fade.recovery(&h.lan, &h.households, now(), true).unwrap(),
            SleepRecovery::Changed
        );
        fade.restore(&h.lan, &h.households, &Fader::default(), &journal)
            .unwrap();
        assert!(
            !h.sim.soap_log()[before..]
                .iter()
                .any(|e| e.action == "SetVolume")
        );
    }
}

#[test]
fn a_failed_durable_checkpoint_prevents_attenuation_and_preserves_the_backstop() {
    let h = House::new();
    let fader = Fader::default();
    let timers = SleepTimers::with_fade(Duration::ZERO);
    timers
        .start(
            &h.lan,
            &h.households,
            &fader,
            &h.coordinator,
            Duration::from_secs(1),
            now(),
        )
        .unwrap();
    let ticket = timers
        .due_runs(now() + chrono::TimeDelta::seconds(1), 1)
        .remove(0);
    let before = h.sim.soap_log().len();
    assert!(
        timers
            .run_due_with_checkpoint(
                &h.lan,
                &h.households,
                &fader,
                &ticket,
                &Journal {
                    fail: true,
                    ..Journal::default()
                }
            )
            .is_err()
    );
    assert_eq!(h.levels(), (30, 20));
    assert!(
        soap::get_remaining_sleep_timer(&h.lan, h.ip("SleepA"))
            .unwrap()
            .is_some()
    );
    assert!(
        !h.sim.soap_log()[before..]
            .iter()
            .any(|e| matches!(e.action.as_str(), "SetVolume" | "Pause"))
    );
}
