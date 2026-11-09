//! Keep reusable mood edits from stranding stored sessions and routines.

use std::collections::{BTreeMap, BTreeSet};

use fsonos_core::dj_direction::DjDirection;
use fsonos_core::scenes::{self, SceneSource};
use fsonos_core::schedule::{self, ScheduleAction};
use fsonos_core::store::Store;

use super::{DjConstraints, Moods};
use crate::SpotifyError;
use crate::steer::DjSession as Session;

/// Keep changed named moods compatible with unexpired sessions, saved scenes,
/// and schedules (including paused ones that the owner may resume). Reject
/// removed definitions and bounds that clash after the existing direction is
/// merged. Changed automatic programs are checked against every distinct mood
/// they may select, without predicting when temporary constraints expire.
/// Unchanged definitions and programs never scan the store.
///
/// Check against the same previous revision that will be compared on save.
/// This reads existing dependencies; the caller owns serialization with DJ
/// session changes. It does not hold database locks or mutate saved routines.
#[allow(clippy::too_many_lines)] // one validation pass reports all session and routine conflicts
pub fn check_references(
    previous: &Moods,
    proposed: &Moods,
    store: &dyn Store,
    now: i64,
) -> Result<(), SpotifyError> {
    let changed: BTreeSet<String> = previous
        .names()
        .chain(proposed.names())
        .filter(|name| previous.get(name) != proposed.get(name))
        .map(str::to_owned)
        .collect();
    let automatic: BTreeSet<&str> = proposed
        .programs()
        .iter()
        .map(|program| program.mood.as_str())
        .collect();
    let automatic_changed = previous.programs() != proposed.programs()
        || automatic.iter().any(|mood| changed.contains(*mood));
    if changed.is_empty() && !automatic_changed {
        return Ok(());
    }
    let mut users: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut mention = |mood: Option<&str>, user: String, result: Result<(), String>| {
        if let Err(reason) = result {
            users
                .entry(mood.map_or_else(
                    || "automatic program".to_owned(),
                    |mood| format!("mood {:?}", mood.to_lowercase()),
                ))
                .or_default()
                .insert(format!("{user} ({reason})"));
        }
    };
    for session in store.dj_sessions()? {
        if session.expires != i64::MAX && now >= session.expires {
            continue;
        }
        if let Some(mood) = &session.mood
            && changed.contains(&mood.to_lowercase())
        {
            let valid = if proposed.get(mood).is_none() {
                Err("its definition would be removed".to_owned())
            } else {
                Session::from_store(&session)
                    .and_then(|session| session.steer(proposed, Some(now)))
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            };
            mention(
                Some(mood),
                format!("session for {:?}", session.coordinator),
                valid,
            );
        } else if session.mood.is_none() && automatic_changed {
            let valid = Session::from_store(&session)
                .map_err(|error| error.to_string())
                .and_then(|session| automatic_valid(proposed, &automatic, &session.constraints));
            mention(
                None,
                format!("session for {:?}", session.coordinator),
                valid,
            );
        }
    }
    let scenes = scenes::list(store).map_err(|error| {
        SpotifyError::Config(format!(
            "cannot verify changed moods against saved scenes: {error}"
        ))
    })?;
    for (scene, _) in scenes {
        for group in scene.groups {
            if let SceneSource::Dj { mood, direction } = group.source {
                let origin = format!("scene {:?}, group {:?}", scene.name, group.coordinator);
                if let Some(mood) = &mood
                    && changed.contains(&mood.to_lowercase())
                {
                    mention(
                        Some(mood),
                        origin,
                        direction_valid(proposed, mood, direction.as_ref()),
                    );
                } else if mood.is_none() && automatic_changed {
                    let valid = direction_constraints(direction.as_ref()).and_then(|constraints| {
                        automatic_valid(proposed, &automatic, &constraints)
                    });
                    mention(None, origin, valid);
                }
            }
        }
    }
    let schedules = schedule::load(store).map_err(|error| {
        SpotifyError::Config(format!(
            "cannot verify changed moods against saved schedules: {error}"
        ))
    })?;
    for schedule in schedules {
        if let ScheduleAction::DjStart {
            mood, direction, ..
        } = schedule.action
        {
            let origin = format!("schedule #{}", schedule.id);
            if let Some(mood) = &mood
                && changed.contains(&mood.to_lowercase())
            {
                mention(
                    Some(mood),
                    origin,
                    direction_valid(proposed, mood, direction.as_ref()),
                );
            } else if mood.is_none() && automatic_changed {
                let valid = direction_constraints(direction.as_ref())
                    .and_then(|constraints| automatic_valid(proposed, &automatic, &constraints));
                mention(None, origin, valid);
            }
        }
    }
    if users.is_empty() {
        return Ok(());
    }
    let dependencies: Vec<String> = users
        .into_iter()
        .map(|(mood, uses)| {
            format!(
                "{mood}: {}",
                uses.into_iter().collect::<Vec<_>>().join(", ")
            )
        })
        .collect();
    Err(SpotifyError::Config(format!(
        "DJ mood changes would invalidate stored settings ({}); change those sessions or routines first",
        dependencies.join("; ")
    )))
}

fn direction_valid(
    moods: &Moods,
    name: &str,
    direction: Option<&DjDirection>,
) -> Result<(), String> {
    let base = moods
        .get(name)
        .ok_or_else(|| "its definition would be removed".to_owned())?;
    base.merged(&direction_constraints(direction)?).validate()
}

fn direction_constraints(direction: Option<&DjDirection>) -> Result<DjConstraints, String> {
    direction
        .map(|direction| {
            serde_json::to_value(&direction.constraints)
                .and_then(serde_json::from_value)
                .map_err(|error| format!("saved direction is unreadable: {error}"))
        })
        .transpose()
        .map(Option::unwrap_or_default)
}

fn automatic_valid(
    moods: &Moods,
    automatic: &BTreeSet<&str>,
    constraints: &DjConstraints,
) -> Result<(), String> {
    // Uncovered windows use the explicit constraints on their own. This also
    // lets disabling the automatic program resolve a previous clash.
    constraints.validate()?;
    for name in automatic {
        let base = moods
            .get(name)
            .ok_or_else(|| format!("automatic program mood {name:?} is missing"))?;
        base.merged(constraints)
            .validate()
            .map_err(|why| format!("automatic program mood {name:?}: {why}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use fsonos_core::scenes::{Scene, SceneGroup};
    use fsonos_core::store::{DjSession, MemStore, StoredScene, StoredSchedule};

    use super::*;
    use crate::steer::DjConstraints;
    use crate::steer::config::{MoodsConfig, ProgramConfig};

    fn custom() -> Moods {
        MoodsConfig {
            moods: BTreeMap::from([("study".into(), DjConstraints::default())]),
            programs: None,
        }
        .resolve()
        .unwrap()
    }

    fn session(expires: i64) -> DjSession {
        DjSession {
            coordinator: "fixture-coordinator".into(),
            mood: Some("STUDY".into()),
            constraints: None,
            expires,
        }
    }

    fn scene(mood: &str) -> Scene {
        Scene {
            name: "Fixture evening".into(),
            groups: vec![SceneGroup {
                coordinator: "Fixture room".into(),
                members: vec![],
                source: SceneSource::Dj {
                    mood: Some(mood.into()),
                    direction: None,
                },
                playing: None,
            }],
            volumes: BTreeMap::new(),
            mutes: BTreeMap::new(),
            writer: None,
        }
    }

    fn scheduled(mood: &str, enabled: bool) -> StoredSchedule {
        StoredSchedule {
            id: 0,
            spec: "daily 18:00".into(),
            action: serde_json::to_string(&ScheduleAction::DjStart {
                room: "Fixture room".into(),
                mood: Some(mood.into()),
                direction: None,
            })
            .unwrap(),
            creator: "cli".into(),
            enabled,
            created: 100,
            last_fired: None,
        }
    }

    fn longer_mood() -> Moods {
        MoodsConfig {
            moods: BTreeMap::from([(
                "study".into(),
                DjConstraints {
                    min_work_minutes: Some(20),
                    ..DjConstraints::default()
                },
            )]),
            programs: None,
        }
        .resolve()
        .unwrap()
    }

    fn directed_store(minimum: Option<u32>) -> MemStore {
        directed_store_for(Some("study"), minimum)
    }

    fn directed_store_for(mood: Option<&str>, minimum: Option<u32>) -> MemStore {
        let mut store = MemStore::default();
        let mut session = session(i64::MAX);
        session.mood = mood.map(str::to_owned);
        session.constraints = Some(
            serde_json::to_string(&DjConstraints {
                min_work_minutes: minimum,
                max_work_minutes: Some(10),
                ..DjConstraints::default()
            })
            .unwrap(),
        );
        store.save_dj_session(&session).unwrap();
        let direction = DjDirection {
            constraints: fsonos_core::dj_direction::DjConstraints {
                min_work_minutes: minimum,
                max_work_minutes: Some(10),
                ..fsonos_core::dj_direction::DjConstraints::default()
            },
            for_secs: Some(3600),
        };
        let mut scene = scene("study");
        scene.groups[0].source = SceneSource::Dj {
            mood: mood.map(str::to_owned),
            direction: Some(direction.clone()),
        };
        scenes::save(&mut store, &scene, 100).unwrap();
        let mut schedule = scheduled("study", false);
        schedule.action = serde_json::to_string(&ScheduleAction::DjStart {
            room: "Fixture room".into(),
            mood: mood.map(str::to_owned),
            direction: Some(direction),
        })
        .unwrap();
        store.add_schedule(&schedule).unwrap();
        store
    }

    fn program(minimum: Option<u32>, enabled: bool) -> Moods {
        MoodsConfig {
            moods: BTreeMap::from([(
                "study".into(),
                DjConstraints {
                    min_work_minutes: minimum,
                    ..DjConstraints::default()
                },
            )]),
            programs: Some(if enabled {
                vec![ProgramConfig {
                    days: vec!["mon".into()],
                    from: "00:00".into(),
                    to: "00:00".into(),
                    mood: "study".into(),
                }]
            } else {
                vec![]
            }),
        }
        .resolve()
        .unwrap()
    }

    #[test]
    fn automatic_program_and_mood_changes_validate_every_retained_direction() {
        let store = directed_store_for(None, None);
        let proposed = program(Some(20), true);
        // One changes only the program, the other only its mood's definition.
        for previous in [longer_mood(), program(None, true)] {
            let error = check_references(&previous, &proposed, &store, 100)
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("automatic program mood \"study\""),
                "{error}"
            );
            assert!(
                error.contains("min_work_minutes 20 exceeds max_work_minutes 10"),
                "{error}"
            );
            assert!(error.contains("session for"), "{error}");
            assert!(error.contains("Fixture evening"), "{error}");
            assert!(error.contains("schedule #"), "{error}");
        }
        assert!(
            check_references(
                &longer_mood(),
                &proposed,
                &directed_store_for(None, Some(1)),
                100,
            )
            .is_ok()
        );
    }

    #[test]
    fn disabling_programs_can_resolve_existing_automatic_bound_conflicts() {
        let store = directed_store_for(None, None);
        assert!(
            check_references(
                &program(Some(20), true),
                &program(Some(20), false),
                &store,
                100
            )
            .is_ok()
        );
    }

    #[test]
    fn expired_automatic_sessions_do_not_block_future_program_changes() {
        let mut store = MemStore::default();
        let mut expired = session(100);
        expired.mood = None;
        expired.constraints = Some("{\"max_work_minutes\":10}".into());
        store.save_dj_session(&expired).unwrap();
        let proposed = program(Some(20), true);
        assert!(check_references(&longer_mood(), &proposed, &store, 99).is_err());
        assert!(check_references(&longer_mood(), &proposed, &store, 100).is_ok());
    }

    #[test]
    fn changed_mood_bounds_cannot_break_active_or_reusable_named_directions() {
        let store = directed_store(None);
        let sessions = store.dj_sessions().unwrap();
        let scenes = store.scenes().unwrap();
        let schedules = store.schedules().unwrap();
        assert!(check_references(&custom(), &custom(), &store, 100).is_ok());
        let error = check_references(&custom(), &longer_mood(), &store, 100)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("min_work_minutes 20 exceeds max_work_minutes 10"),
            "{error}"
        );
        assert!(error.contains("session for"), "{error}");
        assert!(error.contains("Fixture evening"), "{error}");
        assert!(error.contains("schedule #"), "{error}");
        assert_eq!(store.dj_sessions().unwrap(), sessions);
        assert_eq!(store.scenes().unwrap(), scenes);
        assert_eq!(store.schedules().unwrap(), schedules);
    }

    #[test]
    fn explicit_directions_can_override_both_bounds_of_a_changed_mood() {
        let store = directed_store(Some(1));
        assert!(check_references(&custom(), &longer_mood(), &store, 100).is_ok());
        let session = store.dj_sessions().unwrap().pop().unwrap();
        let steering = Session::from_store(&session)
            .unwrap()
            .steer(&longer_mood(), Some(100))
            .unwrap()
            .unwrap();
        assert_eq!(steering.constraints.min_work_minutes, Some(1));
        assert_eq!(steering.constraints.max_work_minutes, Some(10));
    }

    #[test]
    fn removal_identifies_active_sessions_scenes_and_paused_schedules() {
        let mut store = MemStore::default();
        store.save_dj_session(&session(i64::MAX)).unwrap();
        scenes::save(&mut store, &scene("study"), 100).unwrap();
        let schedule = store.add_schedule(&scheduled("Study", false)).unwrap();
        let error = check_references(&custom(), &Moods::builtin(), &store, 100)
            .unwrap_err()
            .to_string();
        assert!(error.contains("study"), "{error}");
        assert!(error.contains("fixture-coordinator"), "{error}");
        assert!(error.contains("Fixture evening"), "{error}");
        assert!(error.contains(&format!("schedule #{schedule}")), "{error}");
        assert!(store.dj_session("fixture-coordinator").unwrap().is_some());
        assert_eq!(store.scenes().unwrap().len(), 1);
        assert_eq!(store.schedules().unwrap().len(), 1);
    }

    #[test]
    fn expired_sessions_do_not_prevent_removing_an_unused_custom_mood() {
        let mut store = MemStore::default();
        let mut expired = session(100);
        expired.constraints = Some("unreadable obsolete constraints".into());
        store.save_dj_session(&expired).unwrap();
        assert!(check_references(&custom(), &Moods::builtin(), &store, 100).is_ok());
        assert!(check_references(&custom(), &Moods::builtin(), &store, 99).is_err());
    }

    #[test]
    fn unchanged_definitions_skip_unrelated_unreadable_rows() {
        let mut store = MemStore::default();
        store
            .save_scene(&StoredScene {
                name: "unreadable".into(),
                spec: "not JSON".into(),
                updated: 100,
            })
            .unwrap();
        assert!(check_references(&Moods::builtin(), &Moods::builtin(), &store, 100).is_ok());
        assert!(check_references(&custom(), &custom(), &store, 100).is_ok());
    }

    #[test]
    fn unrelated_legacy_missing_moods_do_not_block_a_new_removal() {
        let mut store = MemStore::default();
        scenes::save(&mut store, &scene("already-missing"), 100).unwrap();
        store
            .add_schedule(&scheduled("already-missing", true))
            .unwrap();
        assert!(check_references(&custom(), &Moods::builtin(), &store, 100).is_ok());
    }

    #[test]
    fn unreadable_saved_routines_fail_removal_with_an_actionable_source() {
        let mut scenes_store = MemStore::default();
        scenes_store
            .save_scene(&StoredScene {
                name: "broken scene".into(),
                spec: "not JSON".into(),
                updated: 100,
            })
            .unwrap();
        let error = check_references(&custom(), &Moods::builtin(), &scenes_store, 100)
            .unwrap_err()
            .to_string();
        assert!(error.contains("saved scenes"), "{error}");
        assert!(error.contains("broken scene"), "{error}");

        let mut schedules_store = MemStore::default();
        let mut schedule = scheduled("study", true);
        schedule.action = "not JSON".into();
        let id = schedules_store.add_schedule(&schedule).unwrap();
        let error = check_references(&custom(), &Moods::builtin(), &schedules_store, 100)
            .unwrap_err()
            .to_string();
        assert!(error.contains("saved schedules"), "{error}");
        assert!(error.contains(&id.to_string()), "{error}");
    }
}
