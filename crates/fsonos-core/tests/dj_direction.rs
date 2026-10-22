//! Saved DJ direction survives storage, planning and hook dispatch without
//! turning a missing legacy field into an explicit steering reset.

use chrono::{DateTime, Utc};
use fsonos_core::HouseholdState;
use fsonos_core::dj_direction::{DjConstraints, DjDirection};
use fsonos_core::scenes::{
    self, ApplyOptions, PlayingDj, Scene, SceneGroup, SceneHooks, SceneOp, ScenePlan, SceneSource,
};
use fsonos_core::schedule::{self, ScheduleAction, ScheduleSpec};
use fsonos_core::snapshot;
use fsonos_core::store::{SqliteStore, Store};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHousehold, SimModel, SimPlayerSpec};
use fsonos_types::PlayerId;
use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::Mutex;

fn direction() -> DjDirection {
    DjDirection {
        constraints: DjConstraints {
            include_artists: vec!["Nina Simone".into()],
            exclude_genres: vec!["metal".into()],
            decades: vec![1960, 1970],
            include_keywords: vec!["piano".into()],
            min_work_minutes: Some(2),
            max_work_minutes: Some(25),
            energy_bias: -1,
            allow_long_works: true,
            ..DjConstraints::default()
        },
        for_secs: Some(27),
    }
}

fn scene(direction: Option<DjDirection>) -> Scene {
    Scene {
        name: "Piano evening".into(),
        groups: vec![SceneGroup {
            coordinator: "Alpha".into(),
            members: Vec::new(),
            source: SceneSource::Dj {
                mood: Some("calm".into()),
                direction,
            },
            playing: Some(true),
        }],
        volumes: BTreeMap::new(),
        mutes: BTreeMap::new(),
        writer: Some("cli".into()),
    }
}

#[test]
fn missing_and_explicit_automatic_direction_stay_distinct_in_legacy_documents() {
    for (legacy, explicit) in [
        (
            serde_json::json!({"kind":"dj", "mood":"calm"}),
            serde_json::json!({"kind":"dj", "mood":"calm", "direction":{}}),
        ),
        (
            serde_json::json!({"kind":"dj_start", "room":"Alpha", "mood":null}),
            serde_json::json!({"kind":"dj_start", "room":"Alpha", "mood":null, "direction":{}}),
        ),
    ] {
        let encoded = if legacy["kind"] == "dj" {
            let old: SceneSource = serde_json::from_value(legacy.clone()).unwrap();
            let reset: SceneSource = serde_json::from_value(explicit).unwrap();
            assert_ne!(old, reset);
            assert_eq!(serde_json::to_value(old).unwrap(), legacy);
            serde_json::to_value(reset).unwrap()
        } else {
            let old: ScheduleAction = serde_json::from_value(legacy.clone()).unwrap();
            let reset: ScheduleAction = serde_json::from_value(explicit).unwrap();
            assert_ne!(old, reset);
            assert_eq!(serde_json::to_value(old).unwrap(), legacy);
            serde_json::to_value(reset).unwrap()
        };
        assert_eq!(encoded["direction"]["constraints"]["energy_bias"], 0);
        assert!(encoded["direction"].get("for_secs").is_none());
    }
}

#[test]
fn scenes_and_schedules_reopen_with_the_same_relative_direction() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("direction.db");
    let mut store = SqliteStore::open(&path).unwrap();
    let scene = scene(Some(direction()));
    scenes::save(&mut store, &scene, 1_700_000_000).unwrap();
    let spec = ScheduleSpec::from_canonical("daily 07:30").unwrap();
    let action = ScheduleAction::DjStart {
        room: "Alpha".into(),
        mood: Some("calm".into()),
        direction: Some(direction()),
    };
    let now: DateTime<Utc> = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    let id = schedule::add(&mut store, &spec, &action, "cli", now).unwrap();
    store.close().unwrap();

    let mut store = SqliteStore::open(&path).unwrap();
    assert_eq!(scenes::load(&store, "Piano evening").unwrap(), scene);
    let schedules = schedule::load(&store).unwrap();
    assert_eq!(schedules.len(), 1);
    assert_eq!(schedules[0].action, action);
    // Recording a firing changes schedule bookkeeping, never the reusable
    // duration into an absolute expiry or a countdown from the save time.
    store
        .mark_schedule_fired(id, now.timestamp() + 86_400)
        .unwrap();
    assert_eq!(schedule::load(&store).unwrap()[0].action, action);
    store.close().unwrap();
}

struct NoLan;

impl Transport for NoLan {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("DJ hook dispatch must not fall back to a playback write")
    }
}

#[derive(Default)]
struct LegacyHook(Mutex<Vec<Option<String>>>);

impl SceneHooks for LegacyHook {
    fn start_dj(&self, _: &PlayerId, mood: Option<&str>) -> Result<(), String> {
        self.0.lock().unwrap().push(mood.map(str::to_string));
        Ok(())
    }
}

fn dj_plan(direction: Option<DjDirection>) -> ScenePlan {
    ScenePlan {
        ops: vec![SceneOp::Dj {
            coordinator: PlayerId("RINCON_TEST".into()),
            mood: Some("calm".into()),
            direction,
        }],
        notes: Vec::new(),
    }
}

#[test]
fn an_existing_hook_keeps_legacy_behavior() {
    let hook = LegacyHook::default();
    let plan = dj_plan(None);
    let report = scenes::apply(
        &NoLan,
        &[],
        &plan,
        &ApplyOptions {
            fade: None,
            hooks: &hook,
        },
    );
    assert!(report.is_complete(), "{report:?}");
    assert_eq!(report.done, plan.ops);
    assert_eq!(*hook.0.lock().unwrap(), [Some("calm".into())]);
}

#[test]
fn an_existing_hook_refuses_any_explicit_direction_and_blocks_dependent_playback() {
    for direction in [DjDirection::default(), direction()] {
        let hook = LegacyHook::default();
        let mut plan = dj_plan(Some(direction));
        plan.ops.push(SceneOp::Play {
            coordinator: PlayerId("RINCON_TEST".into()),
        });
        let report = scenes::apply(
            &NoLan,
            &[],
            &plan,
            &ApplyOptions {
                fade: None,
                hooks: &hook,
            },
        );
        assert!(!report.is_complete());
        assert!(report.done.is_empty());
        assert_eq!(report.failed.len(), 2);
        assert!(
            report.failed[0]
                .1
                .contains("cannot restore a saved direction")
        );
        assert!(report.failed[1].1.contains("not sent"));
        assert!(hook.0.lock().unwrap().is_empty());
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Started {
    coordinator: PlayerId,
    mood: Option<String>,
    direction: Option<DjDirection>,
}

#[derive(Default)]
struct DirectionHook(Mutex<Vec<Started>>);

impl SceneHooks for DirectionHook {
    fn start_dj(&self, _: &PlayerId, _: Option<&str>) -> Result<(), String> {
        panic!("a direction-aware source must use the new hook")
    }

    fn start_dj_with_direction(
        &self,
        coordinator: &PlayerId,
        mood: Option<&str>,
        direction: Option<&DjDirection>,
    ) -> Result<(), String> {
        self.0.lock().unwrap().push(Started {
            coordinator: coordinator.clone(),
            mood: mood.map(str::to_string),
            direction: direction.cloned(),
        });
        Ok(())
    }
}

#[test]
fn capture_plan_and_dispatch_keep_the_full_direction_in_both_generations() {
    for generation in [1, 2] {
        let model = if generation == 1 {
            SimModel::Play5Gen1
        } else {
            SimModel::One
        };
        let players = [SimPlayerSpec::new("Alpha", model)];
        let builder = SimHousehold::builder();
        let sim = if generation == 1 {
            builder.s1(players)
        } else {
            builder.s2(players)
        }
        .spawn()
        .unwrap();
        let lan = sim.lan();
        let player = sim.player("Alpha").unwrap();
        let coordinator = PlayerId(player.uuid.clone());
        soap::set_av_transport_uri(
            &lan,
            player.ip,
            "x-rincon-mp3radio://example.invalid/test",
            "",
        )
        .unwrap();
        soap::play(&lan, player.ip).unwrap();
        let mut household = HouseholdState::default();
        household.apply_topology(&get_zone_group_state(&lan, player.ip).unwrap());
        let houses = [household];
        let djs = [PlayingDj {
            coordinator: coordinator.clone(),
            mood: Some("calm".into()),
            direction: Some(direction()),
        }];
        let captured = scenes::capture(&lan, &houses, "Piano evening", &djs, 1).unwrap();
        assert_eq!(
            captured.groups[0].source,
            scene(Some(direction())).groups[0].source
        );
        let before = snapshot::capture(&lan, &houses, &coordinator, 2).unwrap();
        let plan = scenes::plan_apply(&captured, &houses, &[before]).unwrap();
        assert_eq!(
            plan.ops,
            [SceneOp::Dj {
                coordinator: coordinator.clone(),
                mood: Some("calm".into()),
                direction: Some(direction()),
            }]
        );
        let hook = DirectionHook::default();
        let options = ApplyOptions {
            fade: None,
            hooks: &hook,
        };
        // Applying the same definition later reapplies the relative duration.
        for _ in 0..2 {
            let report = scenes::apply(&NoLan, &houses, &plan, &options);
            assert!(report.is_complete(), "{report:?}");
        }
        let calls = hook.0.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(calls.iter().all(|call| *call
            == Started {
                coordinator: coordinator.clone(),
                mood: Some("calm".into()),
                direction: Some(direction()),
            }));
    }
}
