use std::collections::BTreeMap;

use fsonos_core::clock::{Clock, FakeClock};
use fsonos_core::playback::PlayerPlayback;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::{Action, ActionFilter, MemStore, Store};
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::PlayerId;
use serde_json::{Value, json};

use super::*;
use crate::dj::{DjEngine, DjMoodsDto, DjSpeakers, DjStatusDto, DjSteer};
use crate::execute::OutcomeDto;
use crate::plan::DjAction;
use crate::surface::dj_config::{
    DjConfigDocument, DjConfigDto, MoodDefinitionDto, ProgramDefinitionDto,
};

struct NoLan;

impl Transport for NoLan {
    fn soap_post(
        &self,
        _: std::net::IpAddr,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<String, ProtoError> {
        panic!("configuration-reference undo must not contact speakers")
    }
}

struct CatalogueDj {
    editable: bool,
    minimum: Option<u32>,
}

impl DjEngine for CatalogueDj {
    fn act(
        &self,
        _: DjSpeakers<'_>,
        _: &mut dyn Store,
        _: DjAction,
        _: &dyn Clock,
    ) -> Result<OutcomeDto, Failure> {
        unreachable!()
    }

    fn steer(
        &self,
        _: DjSpeakers<'_>,
        _: &mut dyn Store,
        _: &DjSteer,
        _: &dyn Clock,
    ) -> Result<OutcomeDto, Failure> {
        unreachable!()
    }

    fn status(
        &self,
        _: DjSpeakers<'_>,
        _: &dyn Store,
        _: Option<u32>,
        _: &dyn Clock,
    ) -> Result<DjStatusDto, Failure> {
        unreachable!()
    }

    fn moods(
        &self,
        _: Option<DjSpeakers<'_>>,
        _: &dyn Store,
        _: &dyn Clock,
    ) -> Result<DjMoodsDto, Failure> {
        unreachable!()
    }

    fn feeds(&self, _: &PlayerId) -> bool {
        false
    }

    fn on_playback(&self, _: DjSpeakers<'_>, _: &mut dyn Store, _: &PlayerPlayback, _: &dyn Clock) {
        unreachable!()
    }

    fn configuration(&self) -> Result<DjConfigDto, Failure> {
        if !self.editable {
            return Err(Failure::new(ErrorCode::NotImplemented, "no configuration"));
        }
        let moods = BTreeMap::from([(
            "calm".into(),
            MoodDefinitionDto {
                min_work_minutes: self.minimum,
                ..MoodDefinitionDto::default()
            },
        )]);
        let programs = self.minimum.map_or_else(Vec::new, |_| {
            vec![ProgramDefinitionDto {
                days: vec!["mon".into()],
                from: "00:00".into(),
                to: "00:00".into(),
                mood: "calm".into(),
            }]
        });
        Ok(DjConfigDto {
            revision: "a".repeat(64),
            config: DjConfigDocument::default(),
            builtin_moods: moods.clone(),
            builtin_programs: programs.clone(),
            moods,
            programs,
        })
    }
}

fn surface(editable: bool) -> Surface {
    Surface::new(
        Box::new(NoLan),
        Box::new(|_| Ok(Vec::new())),
        Policy::default(),
        Box::new(FakeClock::new(
            chrono::DateTime::parse_from_rfc3339("1970-01-01T00:01:40Z").unwrap(),
        )),
    )
    .with_action_log(Box::new(MemStore::default()), "test")
    .with_dj(Box::new(CatalogueDj {
        editable,
        minimum: None,
    }))
}

fn snapshot(kind: &str, mood: &str, expires: i64) -> Value {
    match kind {
        "session" => json!({"dj_sessions": [{
            "coordinator": "fixture-coordinator",
            "row": {
                "coordinator": "fixture-coordinator", "mood": mood,
                "constraints": null, "expires": expires
            }
        }]}),
        "scene" => json!({"scenes": [{
            "was": {
                "name": "Fixture scene", "updated": 50,
                "spec": json!({
                    "name": "Fixture scene", "groups": [{
                        "coordinator": "Fixture room",
                        "source": {"kind": "dj", "mood": mood}
                    }]
                }).to_string()
            },
            "left": null
        }]}),
        "schedule" => json!({"schedules": [{
            "was": {
                "id": 17, "spec": "daily 18:00",
                "action": json!({"kind": "dj_start", "room": "Fixture room", "mood": mood}).to_string(),
                "creator": "cli", "enabled": true, "created": 50, "last_fired": null
            },
            "left_enabled": null
        }]}),
        _ => unreachable!(),
    }
}

fn record(surface: &Surface, before: Value) -> i64 {
    surface
        .with_store(|store| {
            store.record_action(&Action {
                at: 50,
                client: Client::Cli.key().into(),
                surface: "test".into(),
                intent: "synthetic mood-dependent action".into(),
                decision: "allow".into(),
                result: "recorded".into(),
                before_state: Some(before.to_string()),
                undo_of: None,
            })
        })
        .unwrap()
        .unwrap()
}

fn bounds(before: &mut Value, kind: &str, minimum: Option<u32>) {
    let constraints = json!({"min_work_minutes": minimum, "max_work_minutes": 10});
    match kind {
        "session" => {
            before["dj_sessions"][0]["row"]["constraints"] = json!(constraints.to_string());
        }
        "scene" => {
            let mut scene: Value =
                serde_json::from_str(before["scenes"][0]["was"]["spec"].as_str().unwrap()).unwrap();
            scene["groups"][0]["source"]["direction"] = json!({"constraints":constraints});
            before["scenes"][0]["was"]["spec"] = json!(scene.to_string());
        }
        "schedule" => {
            let mut action: Value =
                serde_json::from_str(before["schedules"][0]["was"]["action"].as_str().unwrap())
                    .unwrap();
            action["direction"] = json!({"constraints":constraints});
            before["schedules"][0]["was"]["action"] = json!(action.to_string());
        }
        _ => unreachable!(),
    }
}

fn automatic(before: &mut Value, kind: &str) {
    match kind {
        "session" => before["dj_sessions"][0]["row"]["mood"] = Value::Null,
        "scene" => {
            let mut scene: Value =
                serde_json::from_str(before["scenes"][0]["was"]["spec"].as_str().unwrap()).unwrap();
            scene["groups"][0]["source"]["mood"] = Value::Null;
            before["scenes"][0]["was"]["spec"] = json!(scene.to_string());
        }
        "schedule" => {
            let mut action: Value =
                serde_json::from_str(before["schedules"][0]["was"]["action"].as_str().unwrap())
                    .unwrap();
            action["mood"] = Value::Null;
            before["schedules"][0]["was"]["action"] = json!(action.to_string());
        }
        _ => unreachable!(),
    }
}

fn undo(surface: &Surface, id: i64, checked: bool) -> Result<(), Failure> {
    if checked {
        surface.undo_checked(&Client::Cli, id).map(|_| ())
    } else {
        surface.undo_forced(&Client::Cli, true, true).map(|_| ())
    }
}

fn restored_rows(surface: &Surface) -> (usize, usize, usize, usize) {
    surface
        .with_store(|store| {
            Ok((
                store.dj_sessions()?.len(),
                store.scenes()?.len(),
                store.schedules()?.len(),
                store
                    .recent_actions(&ActionFilter::default())?
                    .iter()
                    .filter(|action| action.action.undo_of.is_some())
                    .count(),
            ))
        })
        .unwrap()
        .unwrap()
}

#[test]
fn deleted_moods_are_rejected_before_checked_or_forced_undo_writes_anything() {
    for checked in [false, true] {
        for kind in ["session", "scene", "schedule"] {
            let surface = surface(true);
            let id = record(&surface, snapshot(kind, "deleted-custom-mood", i64::MAX));
            let failure = undo(&surface, id, checked).unwrap_err();
            assert_eq!(failure.code, ErrorCode::UnknownMood, "{failure:?}");
            assert!(failure.detail.contains("deleted-custom-mood"));
            assert!(failure.detail.contains(kind));
            assert_eq!(restored_rows(&surface), (0, 0, 0, 0));
            assert_eq!(surface.preview_undo(&Client::Cli).unwrap().unwrap().id, id);
        }
    }
}

#[test]
fn known_moods_restore_normally_on_both_undo_paths() {
    for checked in [false, true] {
        for (kind, expected) in [
            ("session", (1, 0, 0, 1)),
            ("scene", (0, 1, 0, 1)),
            ("schedule", (0, 0, 1, 1)),
        ] {
            let surface = surface(true);
            let id = record(&surface, snapshot(kind, "CALM", i64::MAX));
            undo(&surface, id, checked).unwrap();
            assert_eq!(restored_rows(&surface), expected);
        }
    }
}

#[test]
fn changed_mood_bounds_cannot_restore_clashing_directions_but_explicit_bounds_win() {
    for checked in [false, true] {
        for kind in ["session", "scene", "schedule"] {
            for minimum in [None, Some(1)] {
                let surface = surface(true).with_dj(Box::new(CatalogueDj {
                    editable: true,
                    minimum: Some(20),
                }));
                let mut before = snapshot(kind, "calm", i64::MAX);
                bounds(&mut before, kind, minimum);
                let id = record(&surface, before);
                let result = undo(&surface, id, checked);
                if minimum.is_some() {
                    result.unwrap();
                    assert_eq!(restored_rows(&surface).3, 1);
                } else {
                    let failure = result.unwrap_err();
                    assert_eq!(failure.code, ErrorCode::InvalidArgument);
                    assert!(
                        failure
                            .detail
                            .contains("minimum 20 minutes exceeds maximum 10")
                    );
                    assert_eq!(restored_rows(&surface), (0, 0, 0, 0));
                    assert_eq!(surface.preview_undo(&Client::Cli).unwrap().unwrap().id, id);
                }
            }
        }
    }
}

#[test]
fn automatic_undo_directions_are_checked_against_current_program_moods() {
    for checked in [false, true] {
        for kind in ["session", "scene", "schedule"] {
            for constrained in [false, true] {
                let surface = surface(true).with_dj(Box::new(CatalogueDj {
                    editable: true,
                    minimum: Some(20),
                }));
                let mut before = snapshot(kind, "calm", i64::MAX);
                automatic(&mut before, kind);
                if constrained {
                    bounds(&mut before, kind, None);
                }
                let id = record(&surface, before);
                let result = undo(&surface, id, checked);
                if constrained {
                    let failure = result.unwrap_err();
                    assert_eq!(failure.code, ErrorCode::InvalidArgument);
                    assert!(failure.detail.contains("automatic program mood \"calm\""));
                    assert!(
                        failure
                            .detail
                            .contains("minimum 20 minutes exceeds maximum 10")
                    );
                    assert_eq!(restored_rows(&surface), (0, 0, 0, 0));
                } else {
                    result.unwrap();
                    assert_eq!(restored_rows(&surface).3, 1);
                }
            }
        }
    }
}

#[test]
fn expired_session_snapshot_and_older_engines_keep_their_undo_behavior() {
    let surface = surface(true);
    let id = record(&surface, snapshot("session", "deleted-custom-mood", 100));
    surface.undo_checked(&Client::Cli, id).unwrap();
    assert_eq!(restored_rows(&surface), (1, 0, 0, 1));

    let surface = self::surface(false);
    let id = record(
        &surface,
        snapshot("session", "legacy-engine-mood", i64::MAX),
    );
    surface.undo_checked(&Client::Cli, id).unwrap();
    assert_eq!(restored_rows(&surface), (1, 0, 0, 1));
}

#[test]
fn unreadable_referenced_routines_fail_before_restoring_or_consuming_the_action() {
    for kind in ["scene", "schedule"] {
        let surface = surface(true);
        let mut before = snapshot(kind, "calm", i64::MAX);
        if kind == "scene" {
            before["scenes"][0]["was"]["spec"] = json!("not JSON");
        } else {
            before["schedules"][0]["was"]["action"] = json!("not JSON");
        }
        let id = record(&surface, before);
        let failure = surface.undo_checked(&Client::Cli, id).unwrap_err();
        assert_eq!(failure.code, ErrorCode::InvalidArgument);
        assert!(failure.detail.contains(kind));
        assert_eq!(restored_rows(&surface), (0, 0, 0, 0));
        assert_eq!(surface.preview_undo(&Client::Cli).unwrap().unwrap().id, id);
    }
}
