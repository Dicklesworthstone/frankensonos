//! An occurrence survives read-only readiness failures, never uncertain writes.

use super::*;
use crate::surface::Survey;
use fsonos_core::clock::FakeClock;
use fsonos_core::policy::Policy;
use fsonos_core::rooms::Aliases;
use fsonos_core::scenes::Scene;
use fsonos_core::schedule::ScheduleSpec;
use fsonos_core::store::{MemStore, StoredSchedule};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::TransportState;
use std::collections::BTreeMap;
use std::net::IpAddr;
use std::time::Duration;

struct Replies {
    lan: SimLan,
    lose_write: Arc<Mutex<Option<&'static str>>>,
}

impl Transport for Replies {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let reply = self.lan.soap_post(host, path, action, body)?;
        let mut lose = self.lose_write.lock().unwrap();
        if lose.is_some_and(|verb| action.trim_matches('"').ends_with(&format!("#{verb}"))) {
            *lose = None;
            return Err(ProtoError::Network {
                target: "synthetic-scheduled-player".into(),
                detail: "accepted the write but lost the reply".into(),
            });
        }
        Ok(reply)
    }
}

struct House {
    sim: SimHandle,
    surface: Arc<Surface>,
    houses: Vec<HouseholdState>,
    inventory: Arc<Mutex<Result<Vec<HouseholdState>, Failure>>>,
    lose_write: Arc<Mutex<Option<&'static str>>>,
    at: DateTime<Utc>,
}

impl House {
    fn new(s2: bool, action: &ScheduleAction) -> Self {
        let specs = ["Room0", "Room1"].map(|name| {
            SimPlayerSpec::new(
                name,
                if s2 {
                    SimModel::One
                } else {
                    SimModel::Play5Gen1
                },
            )
        });
        let builder = SimHousehold::builder();
        let sim = if s2 {
            builder.s2(specs)
        } else {
            builder.s1(specs)
        }
        .spawn()
        .unwrap();
        let lan = sim.lan();
        let mut household = HouseholdState::default();
        household
            .apply_topology(&get_zone_group_state(&lan, sim.player("Room0").unwrap().ip).unwrap());
        let houses = vec![household];
        let inventory = Arc::new(Mutex::new(Ok(houses.clone())));
        let known = Arc::clone(&inventory);
        let survey: Survey = Box::new(move |_| known.lock().unwrap().clone());
        let lose_write = Arc::new(Mutex::new(None));
        let created = DateTime::parse_from_rfc3339("2026-10-01T10:00:00Z").unwrap();
        let at = created.to_utc() + chrono::TimeDelta::minutes(1);
        let surface = Arc::new(
            Surface::new(
                Box::new(Replies {
                    lan,
                    lose_write: Arc::clone(&lose_write),
                }),
                survey,
                Policy::default(),
                Box::new(FakeClock::new(created)),
            )
            .with_action_log(Box::new(MemStore::default()), "schedule-readiness-test"),
        );
        surface
            .with_store(|store| {
                store.add_schedule(&StoredSchedule {
                    id: 0,
                    spec: ScheduleSpec::Once(at.fixed_offset()).to_string(),
                    action: serde_json::to_string(action).unwrap(),
                    creator: "cli".into(),
                    enabled: true,
                    created: created.timestamp(),
                    last_fired: None,
                })
            })
            .unwrap();
        Self {
            sim,
            surface,
            houses,
            inventory,
            lose_write,
            at,
        }
    }

    fn inventory(&self, next: Result<Vec<HouseholdState>, Failure>) {
        *self.inventory.lock().unwrap() = next;
        self.surface.invalidate();
    }

    fn without(&self, room: &str) -> Vec<HouseholdState> {
        let id = &fsonos_core::resolve_room(&self.houses, room)
            .unwrap()
            .player
            .id;
        let mut houses = self.houses.clone();
        for house in &mut houses {
            house.players.retain(|player| player.id != *id);
            house.rooms.retain(|room| room.primary != *id);
            house.groups.retain(|group| group.coordinator != *id);
        }
        houses
    }

    fn recorded(&self) -> Option<DateTime<Utc>> {
        self.surface.load_schedules().unwrap()[0].last_fired
    }

    fn tick(&self, scheduler: &Scheduler, now: DateTime<Utc>) -> Vec<Ran> {
        let ran = run_schedules(scheduler, now, &Utc);
        assert!(scheduler.wait(Duration::from_secs(5)));
        ran
    }

    fn scene(&self) {
        let scene = Scene {
            name: "Evening".into(),
            groups: Vec::new(),
            volumes: BTreeMap::from([("Room0".into(), 20), ("Room1".into(), 25)]),
            mutes: BTreeMap::new(),
            writer: None,
        };
        self.surface
            .with_store(|store| Ok(scenes::save(store, &scene, self.at.timestamp())))
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

fn pause() -> ScheduleAction {
    ScheduleAction::Pause {
        room: "Room0".into(),
        fade_secs: 0,
    }
}

#[test]
fn due_schedules_wait_for_discovery_and_their_named_room_then_fire_once() {
    for s2 in [false, true] {
        for unavailable in 0..3 {
            let h = House::new(s2, &pause());
            let id = fsonos_core::resolve_room(&h.houses, "Room0")
                .unwrap()
                .player
                .id
                .clone();
            fsonos_core::control::play_uri(
                &h.sim.lan(),
                &h.houses,
                &id,
                "x-rincon-mp3radio://stream.example.invalid/schedule.mp3",
                "",
            )
            .unwrap();
            h.inventory(match unavailable {
                0 => Err(Failure::new(ErrorCode::NotReady, "survey is still running")),
                1 => Ok(Vec::new()),
                _ => Ok(h.without("Room0")),
            });
            let scheduler = Scheduler::new(&h.surface);
            let before = h.sim.soap_log().len();
            h.tick(&scheduler, h.at);
            h.tick(&scheduler, h.at + chrono::TimeDelta::minutes(3));
            assert_eq!(h.recorded(), None);
            assert_eq!(h.sim.soap_log().len(), before);
            h.inventory(Ok(h.houses.clone()));
            h.tick(&scheduler, h.at + chrono::TimeDelta::minutes(6));
            assert_eq!(h.recorded(), Some(h.at));
            assert!(
                h.tick(&scheduler, h.at + chrono::TimeDelta::minutes(7))
                    .is_empty()
            );
            assert_eq!(
                h.sim
                    .soap_log()
                    .iter()
                    .filter(|e| e.action == "Pause")
                    .count(),
                1
            );
            assert_eq!(
                soap::get_transport_info(&h.sim.lan(), h.sim.player("Room0").unwrap().ip)
                    .unwrap()
                    .state,
                TransportState::Paused
            );
        }
    }
}

#[test]
fn unresolved_work_expires_after_grace_without_late_speaker_commands() {
    let h = House::new(true, &pause());
    h.inventory(Err(Failure::new(ErrorCode::NotReady, "LAN unavailable")));
    let scheduler = Scheduler::new(&h.surface);
    let before = h.sim.soap_log().len();
    h.tick(&scheduler, h.at);
    assert_eq!(h.recorded(), None);
    let late = h.at + chrono::TimeDelta::minutes(11);
    assert!(matches!(
        h.tick(&scheduler, late).as_slice(),
        [Ran::Skipped { .. }]
    ));
    assert_eq!(h.recorded(), Some(h.at));
    h.inventory(Ok(h.houses.clone()));
    assert!(h.tick(&scheduler, late).is_empty());
    assert_eq!(h.sim.soap_log().len(), before);
}

#[test]
fn a_scheduled_scene_waits_for_all_named_rooms_before_any_write() {
    for s2 in [false, true] {
        let h = House::new(
            s2,
            &ScheduleAction::ApplyScene {
                name: "Evening".into(),
            },
        );
        h.scene();
        h.inventory(Ok(h.without("Room1")));
        let scheduler = Scheduler::new(&h.surface);
        let before = h.sim.soap_log().len();
        h.tick(&scheduler, h.at);
        assert_eq!(h.recorded(), None);
        assert_eq!(h.sim.soap_log().len(), before);
        h.inventory(Ok(h.houses.clone()));
        h.tick(&scheduler, h.at + chrono::TimeDelta::minutes(1));
        assert_eq!(h.recorded(), Some(h.at));
        for (room, volume) in [("Room0", 20), ("Room1", 25)] {
            assert_eq!(
                soap::get_volume(&h.sim.lan(), h.sim.player(room).unwrap().ip).unwrap(),
                volume
            );
        }
        assert!(
            h.tick(&scheduler, h.at + chrono::TimeDelta::minutes(2))
                .is_empty()
        );
    }
}

#[test]
fn a_partial_scene_with_an_unacknowledged_write_is_never_replayed() {
    let h = House::new(
        true,
        &ScheduleAction::ApplyScene {
            name: "Evening".into(),
        },
    );
    h.scene();
    *h.lose_write.lock().unwrap() = Some("SetVolume");
    let scheduler = Scheduler::new(&h.surface);
    h.tick(&scheduler, h.at);
    assert_eq!(h.recorded(), Some(h.at));
    assert!(h.lose_write.lock().unwrap().is_none());
    assert_eq!(
        soap::get_volume(&h.sim.lan(), h.sim.player("Room0").unwrap().ip).unwrap(),
        20
    );
    let after = h.sim.soap_log().len();
    for minute in 1..4 {
        assert!(
            h.tick(&scheduler, h.at + chrono::TimeDelta::minutes(minute))
                .is_empty()
        );
    }
    assert_eq!(h.sim.soap_log().len(), after);
}

#[test]
fn aliases_and_client_defaults_wait_for_every_explicit_member() {
    let houses = crate::zones::fixtures::households();
    let aliases =
        Aliases::parse("[aliases]\nnight = ['Den', 'Offline room']\n[defaults]\nroom = 'night'\n")
            .unwrap();
    let rooms = plan::Rooms {
        households: &houses,
        aliases: Some(&aliases),
        client: Some("cli"),
    };
    assert!(plan::resolve_set(&rooms, "night").is_ok());
    assert!(targets_ready(&rooms, "night", false).is_err());
    assert!(targets_ready(&rooms, "here", false).is_err());
    let both = Aliases::parse("[aliases]\nkitchens = ['Kitchen']\n").unwrap();
    let rooms = plan::Rooms {
        households: &houses,
        aliases: Some(&both),
        client: Some("cli"),
    };
    assert!(targets_ready(&rooms, "kitchens", false).is_ok());
}
