//! Device-ramp fallback keeps its original intent across real speaker reads.

use super::*;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use std::cell::Cell;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};

fn fixture(s2: bool) -> (SimHandle, Vec<HouseholdState>, PlayerId, IpAddr) {
    let builder = SimHousehold::builder();
    let sim = if s2 {
        builder.s2([SimPlayerSpec::new("Room", SimModel::One)])
    } else {
        builder.s1([SimPlayerSpec::new("Room", SimModel::Play5Gen1)])
    }
    .spawn()
    .unwrap();
    let player = sim.player("Room").unwrap();
    let (id, host) = (PlayerId(player.uuid.clone()), player.ip);
    let lan = sim.lan();
    let mut house = HouseholdState::default();
    house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
    soap::set_volume(&lan, host, 40).unwrap();
    (sim, vec![house], id, host)
}

/// Let a newer manual intent land after GetVolume has read its response but
/// before that response reaches the old ramp. All reads and writes still use
/// the simulator's actual sockets; the interleaving needs no scheduling luck.
struct NewerVolume<'a> {
    inner: &'a SimLan,
    fader: &'a Fader,
    households: &'a [HouseholdState],
    player: &'a PlayerId,
    changed: Cell<bool>,
}

impl Transport for NewerVolume<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let response = self.inner.soap_post(host, path, action, body)?;
        if action.ends_with("#GetVolume\"") && !self.changed.replace(true) {
            self.fader
                .set_volume(self.inner, self.households, self.player, 53)
                .unwrap();
        }
        Ok(response)
    }
}

#[test]
fn device_fallback_never_reclaims_a_newer_manual_volume() {
    for s2 in [false, true] {
        for fault in [401, 402] {
            for cached in [false, true] {
                let (sim, houses, player, host) = fixture(s2);
                let lan = sim.lan();
                let fader = Fader::with_timing(Duration::from_millis(1), Duration::from_millis(1));
                sim.upnp_fault("Room", "RampToVolume", fault).unwrap();
                if cached {
                    assert_eq!(
                        fader
                            .ramp_at_device_speed(&lan, &houses, &player, 40)
                            .unwrap(),
                        RampOutcome::Stepped(FadeOutcome::Reached)
                    );
                }
                let before = sim.soap_log().len();
                let race = NewerVolume {
                    inner: &lan,
                    fader: &fader,
                    households: &houses,
                    player: &player,
                    changed: Cell::new(false),
                };
                let outcome = fader
                    .ramp_at_device_speed(&race, &houses, &player, 10)
                    .unwrap();
                assert!(race.changed.get(), "the newer intent must have landed");
                assert_eq!(
                    outcome,
                    RampOutcome::Stepped(FadeOutcome::Superseded { at: 40 }),
                    "s2={s2}, fault={fault}, cached={cached}"
                );
                assert_eq!(soap::get_volume(&lan, host).unwrap(), 53);
                let log = sim.soap_log();
                let writes: Vec<_> = log[before..]
                    .iter()
                    .filter(|entry| entry.action == "SetVolume")
                    .map(|entry| {
                        entry
                            .args
                            .iter()
                            .find(|(name, _)| name == "DesiredVolume")
                            .unwrap()
                            .1
                            .as_str()
                    })
                    .collect();
                assert_eq!(writes, ["53"], "only the newer intent may write");
                assert_eq!(
                    log[before..]
                        .iter()
                        .filter(|entry| entry.action == "RampToVolume")
                        .count(),
                    usize::from(!cached)
                );

                // A genuinely later request can still use the remembered
                // fallback and reach its target.
                assert_eq!(
                    fader
                        .ramp_at_device_speed(&lan, &houses, &player, 30)
                        .unwrap(),
                    RampOutcome::Stepped(FadeOutcome::Reached)
                );
                assert_eq!(soap::get_volume(&lan, host).unwrap(), 30);
                assert_eq!(
                    sim.soap_log()
                        .iter()
                        .filter(|entry| entry.action == "RampToVolume")
                        .count(),
                    1,
                    "the definite unsupported-action fault stays cached"
                );
            }
        }
    }
}

/// Apply the native ramp, then lose its acknowledgement. Such a result is
/// uncertain: it must never trigger a second, stepped volume operation.
struct LostRampReply<'a> {
    inner: &'a SimLan,
    malformed: bool,
}

impl Transport for LostRampReply<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let response = self.inner.soap_post(host, path, action, body)?;
        if action.ends_with("#RampToVolume\"") {
            if self.malformed {
                return Ok("<incomplete>".into());
            }
            return Err(ProtoError::Network {
                target: "synthetic player".into(),
                detail: "injected lost ramp reply".into(),
            });
        }
        Ok(response)
    }
}

#[test]
fn an_uncertain_native_ramp_is_never_replayed_as_a_fallback() {
    for s2 in [false, true] {
        for malformed in [false, true] {
            let (sim, houses, player, host) = fixture(s2);
            let lan = sim.lan();
            let fader = Fader::default();
            let before = sim.soap_log().len();
            let lost = LostRampReply {
                inner: &lan,
                malformed,
            };
            assert!(
                fader
                    .ramp_at_device_speed(&lost, &houses, &player, 60)
                    .is_err()
            );
            let log = sim.soap_log();
            let actions: Vec<_> = log[before..]
                .iter()
                .map(|entry| entry.action.as_str())
                .collect();
            assert_eq!(actions, ["RampToVolume"]);
            assert_eq!(soap::get_volume(&lan, host).unwrap(), 60);

            // A later explicit request still uses the native action. Losing
            // an acknowledgement is not a capability refusal to cache.
            assert!(matches!(
                fader
                    .ramp_at_device_speed(&lan, &houses, &player, 25)
                    .unwrap(),
                RampOutcome::Device { .. }
            ));
            assert_eq!(soap::get_volume(&lan, host).unwrap(), 25);
            assert_eq!(
                sim.soap_log()
                    .iter()
                    .filter(|entry| entry.action == "RampToVolume")
                    .count(),
                2
            );
        }
    }
}

#[test]
fn other_native_faults_do_not_start_a_fallback() {
    for s2 in [false, true] {
        let (sim, houses, player, host) = fixture(s2);
        let lan = sim.lan();
        let fader = Fader::default();
        sim.upnp_fault("Room", "RampToVolume", 701).unwrap();
        let before = sim.soap_log().len();
        assert!(
            fader
                .ramp_at_device_speed(&lan, &houses, &player, 10)
                .is_err()
        );
        assert_eq!(soap::get_volume(&lan, host).unwrap(), 40);
        assert!(
            sim.soap_log()[before..]
                .iter()
                .all(|entry| entry.action != "SetVolume"),
            "only definite unsupported-action faults authorize stepping"
        );
    }
}

/// Hold the older manual request before it reaches the real speaker, giving
/// an already-started later request a bounded chance to overtake it. The step
/// lock must keep the later request out until this request lands.
struct HeldManual<'a> {
    inner: &'a SimLan,
    entered: Sender<()>,
    attempting: Mutex<Receiver<()>>,
    later_applied: Mutex<Receiver<()>>,
}

impl Transport for HeldManual<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        assert!(action.ends_with("#SetVolume\""));
        self.entered.send(()).unwrap();
        self.attempting
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .expect("the later request must start");
        let later = self
            .later_applied
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(1));
        assert!(matches!(later, Ok(()) | Err(RecvTimeoutError::Timeout)));
        self.inner.soap_post(host, path, action, body)
    }
}

struct LaterWrite<'a> {
    inner: &'a SimLan,
    applied: Sender<()>,
}

impl Transport for LaterWrite<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let response = self.inner.soap_post(host, path, action, body)?;
        if action.ends_with("#SetVolume\"") || action.ends_with("#RampToVolume\"") {
            self.applied.send(()).unwrap();
        }
        Ok(response)
    }
}

#[test]
fn an_older_manual_write_lands_before_a_newer_manual_or_native_intent() {
    for s2 in [false, true] {
        for native in [false, true] {
            let (sim, houses, player, host) = fixture(s2);
            let lan = sim.lan();
            let fader = Fader::default();
            let before = sim.soap_log().len();
            let (entered, ready) = mpsc::channel();
            let (attempting, attempted) = mpsc::channel();
            let (applied, completed) = mpsc::channel();
            let held = HeldManual {
                inner: &lan,
                entered,
                attempting: Mutex::new(attempted),
                later_applied: Mutex::new(completed),
            };
            let later = LaterWrite {
                inner: &lan,
                applied,
            };
            std::thread::scope(|scope| {
                let older = scope.spawn(|| fader.set_volume(&held, &houses, &player, 20));
                ready
                    .recv_timeout(Duration::from_secs(5))
                    .expect("the older manual write must be in flight");
                let newer = scope.spawn(|| {
                    attempting.send(()).unwrap();
                    if native {
                        fader
                            .ramp_at_device_speed(&later, &houses, &player, 70)
                            .map(drop)
                    } else {
                        fader.set_volume(&later, &houses, &player, 70)
                    }
                });
                older.join().unwrap().unwrap();
                newer.join().unwrap().unwrap();
            });
            let log = sim.soap_log();
            let actions: Vec<_> = log[before..]
                .iter()
                .map(|entry| entry.action.as_str())
                .collect();
            let later_action = if native { "RampToVolume" } else { "SetVolume" };
            assert_eq!(actions, ["SetVolume", later_action]);
            let levels: Vec<_> = log[before..]
                .iter()
                .map(|entry| {
                    entry
                        .args
                        .iter()
                        .find(|(name, _)| name == "DesiredVolume")
                        .unwrap()
                        .1
                        .as_str()
                })
                .collect();
            assert_eq!(levels, ["20", "70"], "s2={s2}, native={native}");
            assert_eq!(soap::get_volume(&lan, host).unwrap(), 70);
        }
    }
}
