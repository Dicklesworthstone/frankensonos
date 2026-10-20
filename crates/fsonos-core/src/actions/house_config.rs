//! Undo for the house's configuration: a scene saved over or deleted, a
//! schedule paused, resumed or removed. Each such write logs what it
//! replaced and what it left ([`SceneBefore`], [`ScheduleBefore`]); undo
//! puts the first back, unless someone changed the second since (a scene
//! saved again under the name, a schedule paused or removed): then it
//! refuses, naming the change, and `force` puts it back anyway.

use serde::{Deserialize, Serialize};

use super::{BeforeState, store_err};
use crate::CoreError;
use crate::store::{LoggedAction, Store, StoreError, StoredScene, StoredSchedule};

/// A scene as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SceneRow {
    pub name: String,
    pub spec: String,
    /// When it was saved, in unix seconds.
    pub updated: i64,
}

impl From<StoredScene> for SceneRow {
    fn from(s: StoredScene) -> Self {
        Self {
            name: s.name,
            spec: s.spec,
            updated: s.updated,
        }
    }
}

/// A scene an action saved over or deleted: undo saves `was` back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SceneBefore {
    pub was: SceneRow,
    /// What the action saved in its place; `None` when it deleted it.
    pub left: Option<SceneRow>,
}

/// A schedule as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleRow {
    pub id: i64,
    pub spec: String,
    pub action: String,
    pub creator: String,
    pub enabled: bool,
    pub created: i64,
    pub last_fired: Option<i64>,
}

impl From<StoredSchedule> for ScheduleRow {
    fn from(s: StoredSchedule) -> Self {
        Self {
            id: s.id,
            spec: s.spec,
            action: s.action,
            creator: s.creator,
            enabled: s.enabled,
            created: s.created,
            last_fired: s.last_fired,
        }
    }
}

impl ScheduleRow {
    fn stored(&self) -> StoredSchedule {
        StoredSchedule {
            id: self.id,
            spec: self.spec.clone(),
            action: self.action.clone(),
            creator: self.creator.clone(),
            enabled: self.enabled,
            created: self.created,
            last_fired: self.last_fired,
        }
    }

    /// Whether `s` is this schedule: its id, and what it does when, made
    /// by whom when (a newer schedule may have taken a removed one's id).
    fn is(&self, s: &StoredSchedule) -> bool {
        s.id == self.id
            && s.spec == self.spec
            && s.action == self.action
            && s.creator == self.creator
            && s.created == self.created
    }
}

/// A schedule an action paused, resumed or removed: undo puts `was` back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleBefore {
    pub was: ScheduleRow,
    /// Whether the action left it enabled; `None` when it removed it.
    pub left_enabled: Option<bool>,
}

/// The scene called `name` (ignoring case), as stored now.
///
/// # Errors
/// The store cannot be read.
pub fn scene_row<S: Store + ?Sized>(store: &S, name: &str) -> Result<Option<SceneRow>, StoreError> {
    Ok(store
        .scenes()?
        .into_iter()
        .find(|s| s.name.eq_ignore_ascii_case(name))
        .map(SceneRow::from))
}

/// Schedule `id`, as stored now.
///
/// # Errors
/// The store cannot be read.
pub fn schedule_row<S: Store + ?Sized>(
    store: &S,
    id: i64,
) -> Result<Option<ScheduleRow>, StoreError> {
    Ok(store
        .schedules()?
        .into_iter()
        .find(|s| s.id == id)
        .map(ScheduleRow::from))
}

/// Scenes and schedules as an action's before-state; `None` when there are
/// none.
#[must_use]
pub fn before_state_config(scenes: &[SceneBefore], schedules: &[ScheduleBefore]) -> Option<String> {
    if scenes.is_empty() && schedules.is_empty() {
        return None;
    }
    Some(serde_json::json!({ "scenes": scenes, "schedules": schedules }).to_string())
}

/// Refuse to undo `target` when someone changed the scenes or schedules it
/// left since ([`config_changes`]), as undo refuses for its zones: nothing
/// is put back or logged, so a forced undo still puts back this action.
pub(super) fn refuse_if_changed<S: Store + ?Sized>(
    store: &S,
    target: &LoggedAction,
    before: &BeforeState,
) -> Result<(), CoreError> {
    let changes =
        config_changes(store, &before.scenes, &before.schedules).map_err(|e| store_err(&e))?;
    if changes.is_empty() {
        return Ok(());
    }
    Err(CoreError::NotUndoable(format!(
        "#{} ({}) is left as it is: since it, {}; `fsonos undo --force` puts it back anyway",
        target.id,
        target.action.intent,
        changes.join("; ")
    )))
}

/// What someone changed since in the scenes and schedules an action left,
/// which putting them back would undo: one line each.
fn config_changes<S: Store + ?Sized>(
    store: &S,
    scenes: &[SceneBefore],
    schedules: &[ScheduleBefore],
) -> Result<Vec<String>, StoreError> {
    let mut changes = Vec::new();
    for scene in scenes {
        let name = scene
            .left
            .as_ref()
            .map_or(&scene.was.name, |left| &left.name);
        let now = scene_row(store, name)?;
        let unchanged = match (&scene.left, &now) {
            (None, None) => true,
            (Some(left), Some(now)) => left.name == now.name && left.spec == now.spec,
            _ => false,
        };
        if unchanged {
            continue;
        }
        changes.push(match now {
            Some(now) if scene.left.is_none() => format!("a scene {:?} was saved", now.name),
            Some(now) => format!("the scene {:?} was saved again", now.name),
            None => format!("the scene {name:?} was deleted"),
        });
    }
    for schedule in schedules {
        // A removed schedule overwrites nothing when it comes back.
        let Some(left) = schedule.left_enabled else {
            continue;
        };
        let id = schedule.was.id;
        let now = store.schedules()?.into_iter().find(|s| schedule.was.is(s));
        match now {
            None => changes.push(format!("schedule #{id} was removed")),
            Some(now) if now.enabled != left => changes.push(format!(
                "schedule #{id} was {}",
                if now.enabled { "resumed" } else { "paused" }
            )),
            Some(_) => {}
        }
    }
    Ok(changes)
}

/// What [`put_back_config`] put back and what it could not (with why), each
/// named for people.
#[derive(Debug, Default)]
pub(super) struct PutBack {
    pub(super) done: Vec<String>,
    pub(super) failed: Vec<(String, String)>,
}

impl PutBack {
    /// One line each, for an undo's summary.
    pub(super) fn lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if !self.done.is_empty() {
            lines.push(format!("{} put back", self.done.join(", ")));
        }
        lines.extend(
            self.failed
                .iter()
                .map(|(what, why)| format!("{what} not put back: {why}")),
        );
        lines
    }
}

/// Put back each scene and schedule `before` names as it was; a schedule
/// put back enabled counts as run at `at`, so it runs next at its next time
/// rather than making up the runs it missed.
pub(super) fn put_back_config<S: Store + ?Sized>(
    store: &mut S,
    before: &BeforeState,
    at: i64,
) -> PutBack {
    let mut done = Vec::new();
    let mut failed = Vec::new();
    for scene in &before.scenes {
        let what = format!("the scene {:?}", scene.was.name);
        match put_back_scene(store, scene) {
            Ok(()) => done.push(what),
            Err(e) => failed.push((what, e.to_string())),
        }
    }
    for schedule in &before.schedules {
        let what = format!("schedule #{}", schedule.was.id);
        match put_back_schedule(store, &schedule.was, at) {
            Ok(id) if id == schedule.was.id => done.push(what),
            Ok(id) => done.push(format!("{what} (as #{id})")),
            Err(e) => failed.push((what, e.to_string())),
        }
    }
    PutBack { done, failed }
}

fn put_back_scene<S: Store + ?Sized>(store: &mut S, scene: &SceneBefore) -> Result<(), StoreError> {
    let name = scene
        .left
        .as_ref()
        .map_or(&scene.was.name, |left| &left.name);
    if let Some(now) = scene_row(store, name)? {
        store.delete_scene(&now.name)?;
    }
    store.save_scene(&StoredScene {
        name: scene.was.name.clone(),
        spec: scene.was.spec.clone(),
        updated: scene.was.updated,
    })
}

/// Put `was` back: switched as it was where it still is, under its own id
/// where that is free, else as a new schedule. Returns its id.
fn put_back_schedule<S: Store + ?Sized>(
    store: &mut S,
    was: &ScheduleRow,
    at: i64,
) -> Result<i64, StoreError> {
    let id = if store.schedules()?.iter().any(|s| was.is(s)) {
        store.set_schedule_enabled(was.id, was.enabled)?;
        was.id
    } else if store.put_schedule(&was.stored())? {
        was.id
    } else {
        store.add_schedule(&was.stored())?
    };
    if was.enabled {
        store.mark_schedule_fired(id, at)?;
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemStore;

    fn scene(name: &str, spec: &str, updated: i64) -> StoredScene {
        StoredScene {
            name: name.into(),
            spec: spec.into(),
            updated,
        }
    }

    fn before(scenes: &[SceneBefore], schedules: &[ScheduleBefore]) -> BeforeState {
        BeforeState {
            scenes: scenes.to_vec(),
            schedules: schedules.to_vec(),
            ..BeforeState::default()
        }
    }

    fn daily(creator: &str, created: i64) -> StoredSchedule {
        StoredSchedule {
            id: 0,
            spec: "daily 07:30".into(),
            action: "{\"kind\":\"pause\"}".into(),
            creator: creator.into(),
            enabled: true,
            created,
            last_fired: Some(created),
        }
    }

    #[test]
    fn a_scene_saved_over_or_deleted_comes_back_as_it_was() {
        let mut store = MemStore::default();
        store
            .save_scene(&scene("evening", "{\"v\":1}", 100))
            .unwrap();
        let was = scene_row(&store, "Evening").unwrap().unwrap();
        // Saved over under another case: the new one is the one undo takes out.
        store.delete_scene("evening").unwrap();
        store
            .save_scene(&scene("Evening", "{\"v\":2}", 200))
            .unwrap();
        let over = [SceneBefore {
            was: was.clone(),
            left: scene_row(&store, "Evening").unwrap(),
        }];
        assert!(config_changes(&store, &over, &[]).unwrap().is_empty());
        let put = put_back_config(&mut store, &before(&over, &[]), 300);
        assert!(put.failed.is_empty(), "{put:?}");
        assert_eq!(put.lines(), ["the scene \"evening\" put back"]);
        assert_eq!(
            store.scenes().unwrap(),
            [scene("evening", "{\"v\":1}", 100)]
        );

        // Deleted, then saved again by someone else: undo says so.
        store.delete_scene("evening").unwrap();
        let deleted = [SceneBefore { was, left: None }];
        assert!(config_changes(&store, &deleted, &[]).unwrap().is_empty());
        store
            .save_scene(&scene("EVENING", "{\"v\":3}", 400))
            .unwrap();
        assert_eq!(
            config_changes(&store, &deleted, &[]).unwrap(),
            ["a scene \"EVENING\" was saved"]
        );
        // Forced, it replaces that one.
        put_back_config(&mut store, &before(&deleted, &[]), 500);
        assert_eq!(
            store.scenes().unwrap(),
            [scene("evening", "{\"v\":1}", 100)]
        );
    }

    #[test]
    fn a_removed_or_paused_schedule_comes_back_and_runs_from_now() {
        let mut store = MemStore::default();
        let a = store.add_schedule(&daily("tag:agent", 100)).unwrap();
        let was = schedule_row(&store, a).unwrap().unwrap();

        // Paused: undo turns it on again, counting it as run at the undo.
        store.set_schedule_enabled(a, false).unwrap();
        let paused = [ScheduleBefore {
            was: was.clone(),
            left_enabled: Some(false),
        }];
        assert!(config_changes(&store, &[], &paused).unwrap().is_empty());
        let put = put_back_config(&mut store, &before(&[], &paused), 900);
        assert_eq!(put.done, [format!("schedule #{a}")]);
        let now = schedule_row(&store, a).unwrap().unwrap();
        assert_eq!((now.enabled, now.last_fired), (true, Some(900)));
        // Paused, then resumed by someone else: undo says so.
        store.set_schedule_enabled(a, false).unwrap();
        store.set_schedule_enabled(a, true).unwrap();
        assert_eq!(
            config_changes(&store, &[], &paused).unwrap(),
            [format!("schedule #{a} was resumed")]
        );

        // Removed: it comes back under its own id.
        store.delete_schedule(a).unwrap();
        let removed = [ScheduleBefore {
            was: was.clone(),
            left_enabled: None,
        }];
        assert!(config_changes(&store, &[], &removed).unwrap().is_empty());
        assert_eq!(
            config_changes(&store, &[], &paused).unwrap(),
            [format!("schedule #{a} was removed")]
        );
        put_back_config(&mut store, &before(&[], &removed), 1_000);
        let back = schedule_row(&store, a).unwrap().unwrap();
        assert_eq!(
            back,
            ScheduleRow {
                last_fired: Some(1_000),
                ..was.clone()
            }
        );

        // Removed, and its id taken since: it comes back as a new one.
        store.delete_schedule(a).unwrap();
        let taken = store.add_schedule(&daily("cli", 2_000)).unwrap();
        assert_eq!(taken, a, "the store reuses the newest id");
        let put = put_back_config(&mut store, &before(&[], &removed), 3_000);
        let all = store.schedules().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(put.done, [format!("schedule #{a} (as #{})", all[1].id)]);
        assert_eq!(
            (all[0].creator.as_str(), all[1].creator.as_str()),
            ("cli", "tag:agent")
        );
    }
}
