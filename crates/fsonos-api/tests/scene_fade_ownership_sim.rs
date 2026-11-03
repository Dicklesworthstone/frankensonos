//! A manual API volume change wins while a scene is still loading its source.

use fsonos_api::plan::plan_volume;
use fsonos_api::surface::scenes::{SceneApplyRequest, SceneDefinition};
use fsonos_api::surface::schedules::Sleep;
use fsonos_api::{Surface, VolumeRequest};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::MemStore;
use fsonos_core::{HouseholdState, control, resolve_room};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHousehold, SimLan, SimModel, SimPlayerSpec};
use std::net::IpAddr;
use std::sync::{Mutex, mpsc};
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(5);
const SOURCE: &str = "x-rincon-mp3radio://stream.example.invalid/scene.mp3";

struct Loading {
    lan: SimLan,
    waiting: Mutex<Option<(mpsc::SyncSender<()>, mpsc::Receiver<()>)>>,
}

impl Transport for Loading {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if action.contains("#SetAVTransportURI") && body.contains(SOURCE) {
            let waiting = self.waiting.lock().unwrap().take();
            if let Some((arrived, release)) = waiting {
                arrived.send(()).unwrap();
                release.recv_timeout(WAIT).unwrap();
            }
        }
        self.lan.soap_post(host, path, action, body)
    }
}

fn definition(explicit: bool) -> SceneDefinition {
    let mut definition = serde_json::json!({
        "groups": [{
            "coordinator": "Kitchen",
            "source": { "kind": "uri", "uri": SOURCE }
        }],
        "volumes": { "Office": 30 }
    });
    if explicit {
        definition["volumes"]["Kitchen"] = serde_json::json!(40);
    }
    serde_json::from_value(definition).unwrap()
}

#[test]
fn explicit_and_implicit_scene_fades_preserve_newer_surface_levels() {
    for s2 in [false, true] {
        for explicit in [false, true] {
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
            let kitchen = sim.player("Kitchen").unwrap().ip;
            let office = sim.player("Office").unwrap().ip;
            let mut house = HouseholdState::default();
            house.apply_topology(&get_zone_group_state(&lan, kitchen).unwrap());
            let houses = vec![house];
            let player = resolve_room(&houses, "Kitchen").unwrap().player.id.clone();
            control::play_uri(
                &lan,
                &houses,
                &player,
                "x-rincon-mp3radio://stream.example.invalid/before.mp3",
                "",
            )
            .unwrap();
            soap::pause(&lan, kitchen).unwrap();
            soap::set_volume(&lan, kitchen, 30).unwrap();
            soap::set_volume(&lan, office, 20).unwrap();
            let (arrived, entered) = mpsc::sync_channel(1);
            let (release, waiting) = mpsc::channel();
            let known = houses.clone();
            let surface = Surface::new(
                Box::new(Loading {
                    lan: sim.lan(),
                    waiting: Mutex::new(Some((arrived, waiting))),
                }),
                Box::new(move |_| Ok(known.clone())),
                Policy::default(),
                Box::new(SystemClock),
            )
            .with_sleep(Sleep::default())
            .with_action_log(Box::new(MemStore::default()), "http");
            let definition = definition(explicit);
            surface
                .save_scene_from(&Client::LoopbackHttp, "Evening", &definition)
                .unwrap();

            let report = std::thread::scope(|scope| {
                let scene = scope.spawn(|| {
                    surface.apply_scene_with(
                        &Client::LoopbackHttp,
                        "Evening",
                        &SceneApplyRequest { fade_secs: Some(1) },
                    )
                });
                entered.recv_timeout(WAIT).unwrap();
                assert_eq!(soap::get_volume(&lan, kitchen).unwrap(), 0);
                surface
                    .control(&Client::LoopbackHttp, "set_volume", |rooms| {
                        plan_volume(
                            rooms,
                            &VolumeRequest {
                                zone: "Kitchen".into(),
                                volume: Some(7),
                                delta: None,
                                group: false,
                            },
                        )
                    })
                    .unwrap();
                let from = sim.soap_log().len();
                release.send(()).unwrap();
                let report = scene.join().unwrap().unwrap();
                assert!(
                    sim.soap_log()[from..]
                        .iter()
                        .all(|entry| { entry.room != "Kitchen" || entry.action != "SetVolume" }),
                    "the scene did not regain the manual room's volume"
                );
                report
            });
            assert!(report.complete && report.changed, "{report:?}");
            assert_eq!(soap::get_volume(&lan, kitchen).unwrap(), 7);
            assert_eq!(soap::get_volume(&lan, office).unwrap(), 30);
            assert_eq!(soap::get_media_info(&lan, kitchen).unwrap().uri, SOURCE);
            assert!(
                report.skipped.iter().any(|note| note.contains("Kitchen")),
                "{report:?}"
            );
        }
    }
}
