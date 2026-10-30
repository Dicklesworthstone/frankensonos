//! Scene fades retain their original room intents through preparation and recovery.

use fsonos_core::fade::Fader;
use fsonos_core::scenes::{self, ApplyOptions, SceneOp, ScenePlan, SceneReport};
use fsonos_core::{HouseholdState, control, resolve_room};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::PlayerId;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, mpsc};
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(5);
const SOURCE: &str = "x-rincon-mp3radio://stream.example.invalid/scene.mp3";

struct House {
    sim: SimHandle,
    lan: SimLan,
    houses: Vec<HouseholdState>,
    kitchen: PlayerId,
    office: PlayerId,
    kitchen_host: IpAddr,
    office_host: IpAddr,
}

impl House {
    fn new(s2: bool, grouped: bool) -> Self {
        let model = if s2 {
            SimModel::One
        } else {
            SimModel::Play5Gen1
        };
        let players = [
            SimPlayerSpec::new("Kitchen", model),
            SimPlayerSpec::new("Office", model),
        ];
        let builder = SimHousehold::builder();
        let sim = if s2 {
            builder.s2(players)
        } else {
            builder.s1(players)
        }
        .spawn()
        .unwrap();
        let lan = sim.lan();
        let kitchen_host = sim.player("Kitchen").unwrap().ip;
        let office_host = sim.player("Office").unwrap().ip;
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&lan, kitchen_host).unwrap());
        let mut houses = vec![house];
        let kitchen = resolve_room(&houses, "Kitchen").unwrap().player.id.clone();
        let office = resolve_room(&houses, "Office").unwrap().player.id.clone();
        control::play_uri(
            &lan,
            &houses,
            &kitchen,
            "x-rincon-mp3radio://stream.example.invalid/before.mp3",
            "",
        )
        .unwrap();
        if grouped {
            control::join(&lan, &houses, &office, &kitchen).unwrap();
            houses[0].apply_topology(&get_zone_group_state(&lan, kitchen_host).unwrap());
        }
        soap::pause(&lan, kitchen_host).unwrap();
        soap::set_volume(&lan, kitchen_host, 30).unwrap();
        soap::set_volume(&lan, office_host, 20).unwrap();
        Self {
            sim,
            lan,
            houses,
            kitchen,
            office,
            kitchen_host,
            office_host,
        }
    }

    fn levels(&self) -> (u8, u8) {
        (
            soap::get_volume(&self.lan, self.kitchen_host).unwrap(),
            soap::get_volume(&self.lan, self.office_host).unwrap(),
        )
    }

    fn volumes(&self) -> ScenePlan {
        ScenePlan {
            ops: vec![
                SceneOp::Volume {
                    room: "Kitchen".into(),
                    player: self.kitchen.clone(),
                    level: 40,
                },
                SceneOp::Volume {
                    room: "Office".into(),
                    player: self.office.clone(),
                    level: 50,
                },
            ],
            ..ScenePlan::default()
        }
    }

    fn source(&self, explicit: bool) -> ScenePlan {
        let mut plan = if explicit {
            self.volumes()
        } else {
            ScenePlan::default()
        };
        plan.ops.push(SceneOp::Uri {
            coordinator: self.kitchen.clone(),
            uri: SOURCE.into(),
            metadata: String::new(),
            start: true,
        });
        plan
    }

    fn untouched_since(&self, from: usize, room: &str) {
        assert!(
            self.sim.soap_log()[from..]
                .iter()
                .all(|entry| entry.room != room || entry.action != "SetVolume"),
            "the old scene must never write a superseded room"
        );
    }
}

struct Gate<'a> {
    lan: &'a SimLan,
    host: IpAddr,
    action: &'static str,
    body: &'static str,
    after_write: bool,
    fail: bool,
    waiting: Mutex<Option<(mpsc::SyncSender<()>, mpsc::Receiver<()>)>>,
}

impl<'a> Gate<'a> {
    fn new(
        lan: &'a SimLan,
        host: IpAddr,
        action: &'static str,
        body: &'static str,
    ) -> (Self, mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (arrived, entered) = mpsc::sync_channel(1);
        let (release, waiting) = mpsc::channel();
        (
            Self {
                lan,
                host,
                action,
                body,
                after_write: false,
                fail: false,
                waiting: Mutex::new(Some((arrived, waiting))),
            },
            entered,
            release,
        )
    }
}

fn failed(host: IpAddr) -> ProtoError {
    ProtoError::Network {
        target: host.to_string(),
        detail: "injected scene volume failure".into(),
    }
}

impl Transport for Gate<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let matches = host == self.host
            && action
                .trim_matches('"')
                .ends_with(&format!("#{}", self.action))
            && body.contains(self.body);
        let wait = if matches {
            self.waiting.lock().unwrap().take()
        } else {
            None
        };
        let Some((arrived, release)) = wait else {
            return self.lan.soap_post(host, path, action, body);
        };
        let applied = self
            .after_write
            .then(|| self.lan.soap_post(host, path, action, body));
        arrived.send(()).unwrap();
        release.recv_timeout(WAIT).unwrap();
        if self.fail {
            return Err(failed(host));
        }
        applied.unwrap_or_else(|| self.lan.soap_post(host, path, action, body))
    }
}

fn apply(transport: &dyn Transport, h: &House, plan: &ScenePlan, fader: &Fader) -> SceneReport {
    scenes::apply(
        transport,
        &h.houses,
        plan,
        &ApplyOptions {
            fade: Some((fader, Duration::from_millis(20))),
            ..ApplyOptions::default()
        },
    )
}

#[test]
fn a_manual_level_or_newer_fade_during_source_preparation_keeps_the_room() {
    for s2 in [false, true] {
        for explicit in [false, true] {
            for newer_fade in [false, true] {
                let h = House::new(s2, true);
                let plan = h.source(explicit);
                let fader = Fader::default();
                let (gate, entered, release) =
                    Gate::new(&h.lan, h.kitchen_host, "SetAVTransportURI", SOURCE);
                let report = std::thread::scope(|scope| {
                    let scene = scope.spawn(|| apply(&gate, &h, &plan, &fader));
                    entered.recv_timeout(WAIT).unwrap();
                    assert_eq!(h.levels(), (0, 0), "silence happened before source loading");
                    if newer_fade {
                        fader
                            .fade(&h.lan, &h.houses, &h.kitchen, 7, Duration::ZERO)
                            .unwrap();
                    } else {
                        fader.set_volume(&h.lan, &h.houses, &h.kitchen, 7).unwrap();
                    }
                    let from = h.sim.soap_log().len();
                    release.send(()).unwrap();
                    let report = scene.join().unwrap();
                    h.untouched_since(from, "Kitchen");
                    report
                });
                assert_eq!(h.levels(), (7, if explicit { 50 } else { 20 }));
                assert!(report.is_complete(), "{report:?}");
                assert_eq!(report.faded, 1);
                assert!(report.notes.iter().any(|note| note.contains("Kitchen")));
            }
        }
    }
}

#[test]
fn a_level_changed_during_group_confirmation_is_never_silenced_or_ramped() {
    for s2 in [false, true] {
        for explicit in [false, true] {
            let h = House::new(s2, false);
            let mut plan = h.source(explicit);
            plan.ops.insert(
                0,
                SceneOp::Join {
                    room: "Office".into(),
                    player: h.office.clone(),
                    coordinator: h.kitchen.clone(),
                },
            );
            let fader = Fader::default();
            let (mut gate, entered, release) =
                Gate::new(&h.lan, h.kitchen_host, "GetZoneGroupState", "");
            gate.after_write = true;
            let report = std::thread::scope(|scope| {
                let scene = scope.spawn(|| apply(&gate, &h, &plan, &fader));
                entered.recv_timeout(WAIT).unwrap();
                fader.set_volume(&h.lan, &h.houses, &h.kitchen, 7).unwrap();
                let from = h.sim.soap_log().len();
                release.send(()).unwrap();
                let report = scene.join().unwrap();
                h.untouched_since(from, "Kitchen");
                report
            });
            assert!(report.is_complete(), "{report:?}");
            assert_eq!(h.levels(), (7, if explicit { 50 } else { 20 }));
        }
    }
}

#[test]
fn a_failed_room_does_not_restore_scene_targets_over_a_newer_room_level() {
    for s2 in [false, true] {
        for action in ["GetVolume", "SetVolume"] {
            let h = House::new(s2, true);
            let plan = h.volumes();
            let fader = Fader::default();
            let (mut gate, entered, release) = Gate::new(&h.lan, h.office_host, action, "");
            gate.fail = true;
            let report = std::thread::scope(|scope| {
                let scene = scope.spawn(|| apply(&gate, &h, &plan, &fader));
                entered.recv_timeout(WAIT).unwrap();
                fader.set_volume(&h.lan, &h.houses, &h.kitchen, 7).unwrap();
                let from = h.sim.soap_log().len();
                release.send(()).unwrap();
                let report = scene.join().unwrap();
                h.untouched_since(from, "Kitchen");
                report
            });
            assert_eq!(
                h.levels(),
                (7, 50),
                "the other room recovers its requested level"
            );
            assert!(report.is_complete(), "{report:?}");
            assert!(report.notes.iter().any(|note| note.contains("Kitchen")));
        }
    }
}

/// Fail the fade's initial Office read, then hold Kitchen's recovery write.
struct Recovery<'a> {
    gate: Gate<'a>,
    office: IpAddr,
    failed: AtomicBool,
}

impl Transport for Recovery<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if host == self.office
            && action.contains("#GetVolume")
            && !self.failed.swap(true, Ordering::AcqRel)
        {
            return Err(failed(host));
        }
        self.gate.soap_post(host, path, action, body)
    }
}

#[test]
fn newer_volume_waits_for_an_inflight_scene_recovery_write_then_wins() {
    for s2 in [false, true] {
        let h = House::new(s2, true);
        let plan = h.volumes();
        let fader = Fader::default();
        let (gate, entered, release) = Gate::new(
            &h.lan,
            h.kitchen_host,
            "SetVolume",
            "<DesiredVolume>40</DesiredVolume>",
        );
        let recovery = Recovery {
            gate,
            office: h.office_host,
            failed: AtomicBool::new(false),
        };
        let (report, early) = std::thread::scope(|scope| {
            let scene = scope.spawn(|| apply(&recovery, &h, &plan, &fader));
            entered.recv_timeout(WAIT).unwrap();
            let (began, started) = mpsc::sync_channel(1);
            let (done, finished) = mpsc::channel();
            let change = || fader.set_volume(&h.lan, &h.houses, &h.kitchen, 7);
            let manual = scope.spawn(move || {
                began.send(()).unwrap();
                let result = change();
                done.send(()).unwrap();
                result
            });
            started.recv_timeout(WAIT).unwrap();
            let early = finished.recv_timeout(Duration::from_millis(100)).is_ok();
            release.send(()).unwrap();
            manual.join().unwrap().unwrap();
            (scene.join().unwrap(), early)
        });
        assert!(
            !early,
            "the newer level must land after an old write already in flight"
        );
        assert_eq!(h.levels(), (7, 50));
        assert!(report.is_complete(), "{report:?}");
    }
}

#[test]
fn an_unacknowledged_implicit_silence_keeps_its_recovery_target() {
    for s2 in [false, true] {
        let h = House::new(s2, true);
        let plan = h.source(false);
        let fader = Fader::default();
        let (mut gate, entered, release) = Gate::new(
            &h.lan,
            h.kitchen_host,
            "SetVolume",
            "<DesiredVolume>0</DesiredVolume>",
        );
        gate.after_write = true;
        gate.fail = true;
        let report = std::thread::scope(|scope| {
            let scene = scope.spawn(|| apply(&gate, &h, &plan, &fader));
            entered.recv_timeout(WAIT).unwrap();
            assert_eq!(soap::get_volume(&h.lan, h.kitchen_host).unwrap(), 0);
            release.send(()).unwrap();
            scene.join().unwrap()
        });
        assert!(report.is_complete(), "{report:?}");
        assert_eq!(h.levels(), (30, 20), "no room remains silently stranded");
        assert_eq!(report.faded, 2);
    }
}
