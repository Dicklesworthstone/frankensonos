//! The API's manual-volume hook also retires a failed sleep stop's device timer.

use super::Sleep;
use chrono::{DateTime, Utc};
use fsonos_core::{HouseholdState, control, resolve_room};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::net::IpAddr;
use std::sync::{Mutex, mpsc};
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(5);

struct FailedPause<'a> {
    lan: &'a SimLan,
    entered: mpsc::SyncSender<()>,
    release: Mutex<mpsc::Receiver<()>>,
}

impl Transport for FailedPause<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if action.trim_matches('"').ends_with("#Pause") {
            self.entered.send(()).unwrap();
            self.release.lock().unwrap().recv_timeout(WAIT).unwrap();
            return Err(ProtoError::Network {
                target: host.to_string(),
                detail: "injected failed sleep pause".into(),
            });
        }
        self.lan.soap_post(host, path, action, body)
    }
}

fn playing_room(s2: bool) -> (SimHandle, SimLan, Vec<HouseholdState>, PlayerId, IpAddr) {
    let spec = SimPlayerSpec::new(
        "Kitchen",
        if s2 {
            SimModel::One
        } else {
            SimModel::Play5Gen1
        },
    );
    let builder = SimHousehold::builder();
    let sim = if s2 {
        builder.s2([spec])
    } else {
        builder.s1([spec])
    }
    .spawn()
    .unwrap();
    let lan = sim.lan();
    let host = sim.player("Kitchen").unwrap().ip;
    let mut house = HouseholdState::default();
    house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
    let houses = vec![house];
    let player = resolve_room(&houses, "Kitchen").unwrap().player.id.clone();
    control::play_uri(
        &lan,
        &houses,
        &player,
        "x-rincon-mp3radio://stream.example.invalid/sleep.mp3",
        "",
    )
    .unwrap();
    soap::set_volume(&lan, host, 30).unwrap();
    (sim, lan, houses, player, host)
}

#[test]
fn manual_volume_during_a_failed_sleep_pause_clears_its_old_device_backstop() {
    for s2 in [false, true] {
        let (sim, lan, houses, player, host) = playing_room(s2);
        let sleep = Sleep::default();
        let now = DateTime::parse_from_rfc3339("2026-10-07T22:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        sleep
            .timers
            .start(&lan, &houses, &sleep.fader, &player, Duration::ZERO, now)
            .unwrap();
        assert_eq!(sleep.timers.due(now).len(), 1);
        let (arrived, entered) = mpsc::sync_channel(1);
        let (release, waiting) = mpsc::channel();
        let gate = FailedPause {
            lan: &lan,
            entered: arrived,
            release: Mutex::new(waiting),
        };
        std::thread::scope(|scope| {
            let run = scope.spawn(|| sleep.timers.run(&gate, &houses, &sleep.fader, &player));
            entered.recv_timeout(WAIT).unwrap();
            let (began, started) = mpsc::sync_channel(1);
            let (done, finished) = mpsc::channel();
            let change = || {
                let interrupted = sleep.volume_changed(&houses, &player);
                control::set_volume(&lan, &houses, &player, 50).unwrap();
                interrupted
            };
            let manual = scope.spawn(move || {
                began.send(()).unwrap();
                let interrupted = change();
                done.send(()).unwrap();
                interrupted
            });
            started.recv_timeout(WAIT).unwrap();
            let early = finished.recv_timeout(Duration::from_millis(100)).is_ok();
            release.send(()).unwrap();
            assert!(run.join().unwrap().is_err());
            assert!(
                manual.join().unwrap(),
                "the manual intent interrupted the active sleep run"
            );
            assert!(
                !early,
                "manual volume waits out the Pause already on its way"
            );
        });
        assert_eq!(soap::get_volume(&lan, host).unwrap(), 50);
        assert!(sleep.timers.get(&player).is_none());
        assert_eq!(soap::get_remaining_sleep_timer(&lan, host).unwrap(), None);
        sim.clock().advance(Duration::from_secs(6 * 60));
        assert_eq!(
            soap::get_transport_info(&lan, host).unwrap().state,
            TransportState::Playing
        );
    }
}
