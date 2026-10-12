//! Scenes on every surface: list, show, save, apply and delete the named
//! house states core keeps ([`fsonos_core::scenes`]), alike over HTTP
//! (`/scenes`), the MCP tools (`list_scenes`, `save_scene`, `apply_scene`)
//! and the CLI (`fsonos scene`). A scene is saved from the house as it is,
//! or written out ([`SceneDefinition`]): its groups, what each plays, and
//! room volumes and mutes, checked against the house as it is saved.
//!
//! Applying is a control call like any other: authorize `apply_scene`,
//! snapshot the zones the scene touches, carry out only the steps the house
//! needs (none when it already matches), with each volume bounded by the
//! house policy, and log the action with its before-state, so `undo` puts the
//! house back as it was. Saving and deleting change only the store: they are
//! logged, but there is nothing on the speakers to undo.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::HouseholdState;
use fsonos_core::actions;
use fsonos_core::fade::Fader;
use fsonos_core::favorites;
use fsonos_core::policy::Client;
use fsonos_core::rooms::suggest_rooms;
use fsonos_core::scenes::{
    self, ApplyOptions, PlayingDj, Scene, SceneError, SceneGroup, SceneHooks, SceneOp, ScenePlan,
    SceneReport, SceneSource,
};
use fsonos_core::store::{Store, StoreError};
use fsonos_types::PlayerId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::Duration;

use super::Surface;
use crate::dj::{DjSpeakers, DjSteer, SteerConstraints};
use crate::failure::{ErrorCode, Failure, NoteCode};
use crate::guard::Note;
use crate::plan::{Command, DjAction, VolumeScope};
use crate::request::VolumeChange;
use crate::source::normalize_source_uri;

/// A saved scene (`GET /scenes`, `list_scenes`, `fsonos scene show`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SceneDto {
    pub name: String,
    /// When it was last saved (unix seconds).
    pub saved: i64,
    /// How the rooms group, and what each group plays.
    pub groups: Vec<SceneGroupDto>,
    /// Room → volume (0-100).
    pub volumes: BTreeMap<String, u8>,
    /// Room → muted.
    pub mutes: BTreeMap<String, bool>,
}

/// One group of a scene; a room on its own is a group without members.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SceneGroupDto {
    /// The room that leads the group.
    pub coordinator: String,
    /// The other rooms in it.
    pub members: Vec<String>,
    pub source: SceneSourceDto,
    /// Playing, or else paused or stopped; absent, the group's playback is
    /// left as it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playing: Option<bool>,
}

/// What a scene group plays: `keep` (whatever the group has, its queue
/// say), a `favorite` (by `name`), a `uri` (a stream, or a Spotify track,
/// album or playlist), or the `dj` (in `mood`, if any).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SceneSourceDto {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mood: Option<String>,
}

/// What applying a scene did (`POST /scenes/{name}/apply`, `apply_scene`,
/// `fsonos scene apply`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SceneApplyDto {
    pub scene: String,
    /// What was done, in a sentence.
    pub done: String,
    /// Whether anything was sent to a speaker (false: the house already
    /// matched the scene).
    pub changed: bool,
    /// Whether every step went through.
    pub complete: bool,
    /// The steps carried out, in order.
    pub steps: Vec<String>,
    /// The steps that failed, with why; the rest still ran.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed: Vec<SceneStepFailureDto>,
    /// Rooms the scene names that are not in the house now.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<String>,
    /// Volumes the house policy lowered or left alone (`VOLUME_CLAMPED`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<Note>,
}

/// A scene written out rather than saved from the house as it is (the
/// `PUT /scenes/{name}` body, `save_scene`'s `scene`, `fsonos scene save
/// --from`). Rooms are named as the house names them (any case) and must be
/// in it now; `volumes` and `mutes` leave the rooms they don't name alone.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SceneDefinition {
    /// How the rooms group, and what each group plays.
    #[serde(default)]
    pub groups: Vec<SceneGroupDefinition>,
    /// Room → volume (0-100).
    #[serde(default)]
    pub volumes: BTreeMap<String, u8>,
    /// Room → muted.
    #[serde(default)]
    pub mutes: BTreeMap<String, bool>,
}

/// One group of a [`SceneDefinition`]; a room on its own is a group without
/// members.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SceneGroupDefinition {
    /// The room that leads the group.
    pub coordinator: String,
    /// The other rooms in it.
    #[serde(default)]
    pub members: Vec<String>,
    /// What it plays: `keep` (whatever it has; the default), a `favorite`
    /// (`name`), a stream or file `uri` (Spotify plays through a favorite or
    /// the DJ), or the `dj` (`mood`, optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SceneSourceDto>,
    /// Playing, or else paused. By default a group with a source other than
    /// `keep` plays, and a `keep` group's playback is left as it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playing: Option<bool>,
}

impl SceneDefinition {
    /// The scene `name` this defines, its rooms named as the house names
    /// them, or why it can't be one (a room not in the house, a source that
    /// can't play, two households in one group).
    fn scene(&self, name: &str, households: &[HouseholdState]) -> Result<Scene, Failure> {
        let room = |asked: &str| -> Result<String, Failure> {
            let rooms = || households.iter().flat_map(|h| &h.rooms);
            rooms()
                .find(|r| r.name.eq_ignore_ascii_case(asked.trim()))
                .map(|r| r.name.clone())
                .ok_or_else(|| {
                    let known: Vec<String> = rooms().map(|r| r.name.clone()).collect();
                    Failure::new(
                        ErrorCode::UnknownRoom,
                        format!("no room {asked:?} in the house"),
                    )
                    .with_suggestions(suggest_rooms(asked, &known))
                })
        };
        let mut groups = Vec::with_capacity(self.groups.len());
        for g in &self.groups {
            let source = g
                .source
                .as_ref()
                .map(source_of)
                .transpose()?
                .unwrap_or_default();
            groups.push(SceneGroup {
                coordinator: room(&g.coordinator)?,
                members: g
                    .members
                    .iter()
                    .map(|m| room(m))
                    .collect::<Result<_, _>>()?,
                playing: g
                    .playing
                    .or((!matches!(source, SceneSource::Keep)).then_some(true)),
                source,
            });
        }
        let mut volumes = BTreeMap::new();
        for (asked, level) in &self.volumes {
            if *level > 100 {
                return Err(Failure::invalid(format!(
                    "the volume for {asked} must be 0 to 100, not {level}"
                )));
            }
            volumes.insert(room(asked)?, *level);
        }
        let mut mutes = BTreeMap::new();
        for (asked, muted) in &self.mutes {
            mutes.insert(room(asked)?, *muted);
        }
        let scene = Scene {
            name: name.trim().to_string(),
            groups,
            volumes,
            mutes,
            writer: None,
        };
        scenes::check(&scene, households).map_err(scene_failure)?;
        Ok(scene)
    }
}

/// What a written-out group plays, or why that can't be a scene's source.
fn source_of(dto: &SceneSourceDto) -> Result<SceneSource, Failure> {
    let needs =
        |field: &str| Failure::invalid(format!("a {} source needs its `{field}`", dto.kind));
    match dto.kind.as_str() {
        "keep" => Ok(SceneSource::Keep),
        "favorite" => {
            let name = dto.name.as_deref().map(str::trim).filter(|n| !n.is_empty());
            Ok(SceneSource::Favorite {
                name: name.ok_or_else(|| needs("name"))?.to_string(),
                uri: None,
            })
        }
        "uri" => {
            let uri = normalize_source_uri(dto.uri.as_deref().ok_or_else(|| needs("uri"))?)?;
            let playable = uri.starts_with("spotify:track:")
                || fsonos_proto::didl::SpotifyContainer::of(&uri).is_some();
            if uri.starts_with("spotify:") && !playable {
                return Err(Failure::invalid(format!(
                    "a scene plays a Spotify track, album or playlist, not {uri}"
                ))
                .with_hint("Name one of its tracks, albums or playlists, or use the dj source."));
            }
            Ok(SceneSource::Uri {
                uri,
                metadata: String::new(),
            })
        }
        "dj" => Ok(SceneSource::Dj {
            mood: dto.mood.clone(),
        }),
        other => Err(Failure::invalid(format!(
            "no scene source {other:?}: use keep, favorite, uri or dj"
        ))),
    }
}

/// `POST /scenes/{name}/apply` body (`apply_scene`): with `fade_secs`, the
/// scene's volumes ramp there over that long (1-60 s) instead of at once.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SceneApplyRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fade_secs: Option<u32>,
}

/// A step of applying a scene that failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SceneStepFailureDto {
    pub step: String,
    pub error: String,
}

/// What deleting a scene did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SceneDeletedDto {
    pub deleted: String,
    pub done: String,
}

impl SceneDto {
    #[must_use]
    pub fn new(scene: &Scene, saved: i64) -> Self {
        Self {
            name: scene.name.clone(),
            saved,
            groups: scene
                .groups
                .iter()
                .map(|g| SceneGroupDto {
                    coordinator: g.coordinator.clone(),
                    members: g.members.clone(),
                    source: SceneSourceDto::from(&g.source),
                    playing: g.playing,
                })
                .collect(),
            volumes: scene.volumes.clone(),
            mutes: scene.mutes.clone(),
        }
    }
}

impl From<&SceneSource> for SceneSourceDto {
    fn from(source: &SceneSource) -> Self {
        let dto = |kind: &str| Self {
            kind: kind.to_string(),
            name: None,
            uri: None,
            mood: None,
        };
        match source {
            SceneSource::Keep => dto("keep"),
            SceneSource::Favorite { name, uri } => Self {
                name: Some(name.clone()),
                uri: uri.clone(),
                ..dto("favorite")
            },
            SceneSource::Uri { uri, .. } => Self {
                uri: Some(uri.clone()),
                ..dto("uri")
            },
            SceneSource::Dj { mood } => Self {
                mood: mood.clone(),
                ..dto("dj")
            },
        }
    }
}

/// The failure a surface reports for `err`.
#[must_use]
pub fn scene_failure(err: SceneError) -> Failure {
    let detail = err.to_string();
    match err {
        SceneError::BadName | SceneError::RoomTwice(_) => Failure::invalid(detail),
        SceneError::CrossHousehold { .. } => Failure::new(ErrorCode::CrossHouseholdGroup, detail)
            .with_hint(
                "Save the scene again from the house as it is now; S1 and S2 rooms can never \
                 share a group.",
            ),
        SceneError::Unknown { name, known } => {
            let nearest = suggest_rooms(&name, &known);
            Failure::new(ErrorCode::UnknownScene, detail).with_suggestions(if nearest.is_empty() {
                known
            } else {
                nearest
            })
        }
        SceneError::Corrupt { .. } => Failure::new(ErrorCode::Internal, detail)
            .with_hint("Save the scene again (fsonos scene save), or delete it."),
        SceneError::Store(e) => store_failure(&e),
        SceneError::Core(e) => Failure::from(e),
    }
}

/// Store errors can carry a DB path or engine internals: they go to the log,
/// not to the caller.
fn store_failure(err: &StoreError) -> Failure {
    tracing::warn!("scene store: {err}");
    Failure::new(ErrorCode::Internal, "internal error")
}

/// The room `player` belongs to, for the steps a caller reads.
fn room_of(households: &[HouseholdState], player: &PlayerId) -> String {
    households
        .iter()
        .flat_map(|h| &h.rooms)
        .find(|r| r.primary == *player || r.players.contains(player))
        .map_or_else(|| player.0.clone(), |r| r.name.clone())
}

/// `op` in words.
fn describe(households: &[HouseholdState], op: &SceneOp) -> String {
    let room = |p: &PlayerId| room_of(households, p);
    match op {
        SceneOp::Leave { room, .. } => format!("{room} leaves its group"),
        SceneOp::Join {
            room: member,
            coordinator,
            ..
        } => format!("{member} joins {}", room(coordinator)),
        SceneOp::Volume { room, level, .. } => format!("{room} volume {level}"),
        SceneOp::Mute { room, mute, .. } => {
            format!("{room} {}", if *mute { "muted" } else { "unmuted" })
        }
        SceneOp::Favorite { coordinator, name } => {
            format!("{} plays the favorite {name}", room(coordinator))
        }
        SceneOp::Uri {
            coordinator, uri, ..
        } => format!("{} plays {uri}", room(coordinator)),
        SceneOp::Dj { coordinator, mood } => match mood {
            Some(mood) => format!("{} starts the DJ ({mood})", room(coordinator)),
            None => format!("{} starts the DJ", room(coordinator)),
        },
        SceneOp::Play { coordinator } => format!("{} plays", room(coordinator)),
        SceneOp::Pause { coordinator } => format!("{} pauses", room(coordinator)),
    }
}

/// What applying the scene `scene` did, as the surfaces show it.
fn applied(
    scene: &str,
    households: &[HouseholdState],
    report: SceneReport,
    notes: Vec<Note>,
) -> SceneApplyDto {
    let steps: Vec<String> = report
        .done
        .iter()
        .map(|op| describe(households, op))
        .collect();
    let failed: Vec<SceneStepFailureDto> = report
        .failed
        .iter()
        .map(|(op, error)| SceneStepFailureDto {
            step: describe(households, op),
            error: error.clone(),
        })
        .collect();
    let mut done = if failed.is_empty() {
        format!("applied scene {scene} ({} step(s))", steps.len())
    } else {
        format!(
            "applied scene {scene} in part: {} step(s) done, {} failed",
            steps.len(),
            failed.len()
        )
    };
    if !report.notes.is_empty() {
        let _ = write!(done, "; {}", report.notes.join("; "));
    }
    SceneApplyDto {
        scene: scene.to_string(),
        done,
        changed: !report.done.is_empty(),
        complete: failed.is_empty(),
        steps,
        failed,
        skipped: report.notes,
        notes,
    }
}

/// The DJ a scene starts, through the surface's engine and store.
struct SceneDj<'a> {
    surface: &'a Surface,
    households: &'a [HouseholdState],
}

impl SceneHooks for SceneDj<'_> {
    fn start_dj(&self, coordinator: &PlayerId, mood: Option<&str>) -> Result<(), String> {
        let s = self.surface;
        let dj = s.dj.as_deref().ok_or("this surface runs no DJ")?;
        let at = DjSpeakers {
            transport: &*s.transport,
            households: self.households,
            coordinator,
        };
        let started = s.with_dj_store(|store| {
            if let Some(mood) = mood {
                let steer = DjSteer::Set {
                    mood: Some(mood.to_lowercase()),
                    constraints: SteerConstraints::default(),
                    // Until the time-of-day program takes over again.
                    for_secs: s.mood_lasts(),
                };
                if let Err(refused) = dj.steer(at, store, &steer, &*s.clock) {
                    return Ok(Err(refused));
                }
            }
            Ok(dj.act(at, store, DjAction::Start, &*s.clock))
        });
        match started {
            Ok(Some(Ok(_))) => Ok(()),
            Ok(Some(Err(f))) | Err(f) => Err(f.detail),
            Ok(None) => Err("the DJ needs the daemon's store".into()),
        }
    }
}

impl Surface {
    /// Run `f` on the store the action log keeps, which holds the scenes.
    fn scene_store<R>(
        &self,
        f: impl FnOnce(&mut dyn Store) -> Result<R, SceneError>,
    ) -> Result<R, Failure> {
        let Some(log) = &self.log else {
            return Err(Failure::new(
                ErrorCode::NotImplemented,
                "scenes need the daemon's store, which this surface does not keep",
            )
            .with_hint("Set a data directory (FSONOS_DATA_DIR), or ask the daemon."));
        };
        let mut store = log
            .store
            .lock()
            .map_err(|_| Failure::new(ErrorCode::Internal, "store poisoned"))?;
        f(&mut **store).map_err(scene_failure)
    }

    /// The households, or `NOT_READY` when no room answered.
    fn scene_house(&self) -> Result<Vec<HouseholdState>, Failure> {
        let households = self.households()?;
        if households.iter().all(|h| h.rooms.is_empty()) {
            return Err(Failure::new(ErrorCode::NotReady, "no Sonos rooms answered"));
        }
        Ok(households)
    }

    /// Every saved scene, by name (`list_scenes`, read-only).
    pub fn scenes(&self, client: &Client) -> Result<Vec<SceneDto>, Failure> {
        self.guard(client).authorize("list_scenes", true)?;
        let listed = self.scene_store(|store| scenes::list(store))?;
        Ok(listed.iter().map(|(s, at)| SceneDto::new(s, *at)).collect())
    }

    /// The scene called `name`, ignoring case (`get_scene`, read-only).
    pub fn scene(&self, client: &Client, name: &str) -> Result<SceneDto, Failure> {
        self.guard(client).authorize("get_scene", true)?;
        let (scene, saved) = self.scene_store(|store| {
            let scene = scenes::load(store, name)?;
            let saved = store
                .scenes()?
                .iter()
                .find(|s| s.name == scene.name)
                .map_or(0, |s| s.updated);
            Ok((scene, saved))
        })?;
        Ok(SceneDto::new(&scene, saved))
    }

    /// Save the house as it is now as the scene `name`, replacing one of
    /// that name (`save_scene`).
    pub fn save_scene(&self, client: &Client, name: &str) -> Result<SceneDto, Failure> {
        let tool = "save_scene";
        self.authorize_logged(client, tool)?;
        // A bad name fails before the speakers are read.
        let length = name.trim().chars().count();
        if length == 0 || length > scenes::MAX_NAME {
            return Err(scene_failure(SceneError::BadName));
        }
        let households = self.scene_house()?;
        let now = self.now();
        let sessions = self.scene_store(|store| Ok(store.dj_sessions()?))?;
        // Where the DJ plays now, with its steering's mood. A session alone
        // is only steering, kept whether or not the DJ runs.
        let djs: Vec<PlayingDj> = households
            .iter()
            .flat_map(|h| &h.groups)
            .filter(|g| self.dj.as_ref().is_some_and(|dj| dj.feeds(&g.coordinator)))
            .map(|g| PlayingDj {
                coordinator: g.coordinator.clone(),
                mood: sessions
                    .iter()
                    .find(|s| s.coordinator == g.coordinator.0 && s.expires > now)
                    .and_then(|s| s.mood.clone()),
            })
            .collect();
        let scene = scenes::capture(&*self.transport, &households, name, &djs, now)
            .map_err(|e| self.explain(scene_failure(e)))?;
        self.store_scene(client, tool, &scene)
    }

    /// Save `def` as the scene `name` (`save_scene` with a scene): checked
    /// against the house as it is now, each favorite found in its group's
    /// household, rather than captured from the speakers.
    pub fn save_scene_from(
        &self,
        client: &Client,
        name: &str,
        def: &SceneDefinition,
    ) -> Result<SceneDto, Failure> {
        let tool = "save_scene";
        self.authorize_logged(client, tool)?;
        let length = name.trim().chars().count();
        if length == 0 || length > scenes::MAX_NAME {
            return Err(scene_failure(SceneError::BadName));
        }
        if *def == SceneDefinition::default() {
            return Err(
                Failure::invalid("the scene written out names no group, volume or mute").with_hint(
                    "Name at least one, or leave the scene out to save the house as it is.",
                ),
            );
        }
        let households = self.scene_house()?;
        let mut scene = def.scene(name, &households)?;
        for group in &mut scene.groups {
            if let SceneSource::Dj { mood: Some(mood) } = &mut group.source {
                *mood = self.scene_mood(mood)?;
                continue;
            }
            let SceneSource::Favorite { name, uri } = &mut group.source else {
                continue;
            };
            let Some(lead) = households
                .iter()
                .flat_map(|h| &h.rooms)
                .find(|r| r.name == group.coordinator)
            else {
                continue;
            };
            let found = favorites::list(&*self.transport, &households, &lead.primary)
                .map_err(Failure::from)
                .inspect_err(|f| self.notice(f))?;
            let favorite = favorites::find(&found, name)?;
            name.clone_from(&favorite.title);
            uri.clone_from(&favorite.uri);
        }
        self.store_scene(client, tool, &scene)
    }

    /// `mood` as the DJ names it, or `UNKNOWN_MOOD`, where this surface runs
    /// the DJ; elsewhere `mood` as written (applying the scene finds out).
    fn scene_mood(&self, mood: &str) -> Result<String, Failure> {
        let Some(dj) = self.dj.as_deref() else {
            return Ok(mood.to_string());
        };
        let known = self.with_dj_store(|store| Ok(dj.moods(None, store, &*self.clock)))?;
        let Some(Ok(known)) = known else {
            return Ok(mood.to_string());
        };
        if let Some(named) = known
            .moods
            .iter()
            .find(|m| m.name.eq_ignore_ascii_case(mood.trim()))
        {
            return Ok(named.name.clone());
        }
        Err(Failure::new(
            ErrorCode::UnknownMood,
            format!("no DJ mood is called {mood:?}"),
        )
        .with_suggestions(known.moods.into_iter().map(|m| m.name)))
    }

    /// Keep `scene`, replacing one of the same name in any case, and log it.
    fn store_scene(&self, client: &Client, tool: &str, scene: &Scene) -> Result<SceneDto, Failure> {
        // Who saved it bounds its volumes when anyone applies it.
        let scene = &Scene {
            writer: Some(client.key().to_string()),
            ..scene.clone()
        };
        let now = self.now();
        let replaced = self.scene_store(|store| {
            let replaced = store.scenes()?.iter().find_map(|s| {
                s.name
                    .eq_ignore_ascii_case(&scene.name)
                    .then(|| s.name.clone())
            });
            if let Some(old) = replaced.as_deref().filter(|old| *old != scene.name) {
                store.delete_scene(old)?;
            }
            scenes::save(store, scene, now)?;
            Ok(replaced.is_some())
        })?;
        let rooms = scene.volumes.len();
        let done = format!(
            "{} scene {} ({} group(s), {rooms} room(s))",
            if replaced { "replaced" } else { "saved" },
            scene.name,
            scene.groups.len()
        );
        self.record(
            client,
            format!("{tool}: {}", scene.name),
            "allow".into(),
            done,
            None,
        );
        Ok(SceneDto::new(scene, now))
    }

    /// Forget the scene called `name` (`delete_scene`).
    pub fn delete_scene(&self, client: &Client, name: &str) -> Result<SceneDeletedDto, Failure> {
        let tool = "delete_scene";
        self.authorize_logged(client, tool)?;
        let deleted = self.scene_store(|store| {
            let scene = scenes::load(store, name)?;
            scenes::delete(store, &scene.name)?;
            Ok(scene.name)
        })?;
        let done = format!("deleted scene {deleted}");
        self.record(
            client,
            format!("{tool}: {deleted}"),
            "allow".into(),
            done.clone(),
            None,
        );
        Ok(SceneDeletedDto { deleted, done })
    }

    /// Put the house in the scene `name` (`apply_scene`): only the steps it
    /// needs, volumes within the house policy, logged with the before-state
    /// so `undo` restores the house.
    pub fn apply_scene(&self, client: &Client, name: &str) -> Result<SceneApplyDto, Failure> {
        self.apply_scene_fading(client, name, None)
    }

    /// [`Self::apply_scene`], its volumes ramping over `req.fade_secs` when
    /// given (1-60 s; the call answers once the fade is done).
    pub fn apply_scene_with(
        &self,
        client: &Client,
        name: &str,
        req: &SceneApplyRequest,
    ) -> Result<SceneApplyDto, Failure> {
        let fade = req.fade_secs.map(super::schedules::call_fade).transpose()?;
        self.apply_scene_fading(client, name, fade)
    }

    /// The daemon's fader for applying `plan`, once every fade in flight on
    /// the rooms whose volume it sets has ended (`volumes_changing`), so none
    /// overwrites the scene's levels a step later.
    fn fader_for<R>(
        &self,
        households: &[HouseholdState],
        plan: &ScenePlan,
        f: impl FnOnce(&Fader) -> R,
    ) -> R {
        let set: Vec<PlayerId> = plan
            .ops
            .iter()
            .filter_map(|op| match op {
                SceneOp::Volume { player, .. } => Some(player.clone()),
                _ => None,
            })
            .collect();
        self.volumes_changing(households, &set);
        self.with_fader(f)
    }

    /// [`Self::apply_scene`], fading its volumes over `fade` with the
    /// daemon's fader (so a volume change meanwhile ends the fade where it
    /// is).
    pub(super) fn apply_scene_fading(
        &self,
        client: &Client,
        name: &str,
        fade: Option<Duration>,
    ) -> Result<SceneApplyDto, Failure> {
        let tool = "apply_scene";
        self.authorize_logged(client, tool)?;
        let scene = self.scene_store(|store| scenes::load(store, name))?;
        let households = self.scene_house()?;
        let zones = scenes::zones(&scene, &households);
        let (snaps, missed) =
            actions::capture_zones(&*self.transport, &households, &zones, self.now());
        let plan = scenes::plan_apply(&scene, &households, &snaps).map_err(scene_failure)?;
        let (plan, mut notes) = self.bound_volumes(client, &households, plan);
        // And for whoever saved it: a capped client's scene stays within its
        // caps when the CLI, or a schedule made from it, applies it.
        let writer = scene
            .writer
            .as_deref()
            .map(super::schedules::client_of)
            .filter(|writer| writer != client);
        let plan = match writer {
            Some(writer) => {
                let (plan, more) = self.bound_volumes(&writer, &households, plan);
                notes.extend(more);
                plan
            }
            None => plan,
        };
        if plan.ops.is_empty() {
            return Ok(SceneApplyDto {
                scene: scene.name.clone(),
                done: format!("the house already matches scene {}", scene.name),
                changed: false,
                complete: true,
                steps: Vec::new(),
                failed: Vec::new(),
                skipped: plan.notes,
                notes,
            });
        }
        // A scene that starts the DJ changes its stored steering too.
        let dj_zones: Vec<PlayerId> = plan
            .ops
            .iter()
            .filter_map(|op| match op {
                SceneOp::Dj { coordinator, .. } => Some(coordinator.clone()),
                _ => None,
            })
            .collect();
        let sessions = if dj_zones.is_empty() || self.log.is_none() {
            Vec::new()
        } else {
            self.scene_store(|store| Ok(actions::capture_sessions(store, &dj_zones)?))?
        };
        let hooks = SceneDj {
            surface: self,
            households: &households,
        };
        let report = self.fader_for(&households, &plan, |fader| {
            scenes::apply(
                &*self.transport,
                &households,
                &plan,
                &ApplyOptions {
                    fade: fade.map(|over| (fader, over)),
                    hooks: &hooks,
                },
            )
        });
        let faded = fade.filter(|_| report.faded > 0);
        let regrouped = report
            .done
            .iter()
            .chain(report.failed.iter().map(|(op, _)| op))
            .any(|op| matches!(op, SceneOp::Leave { .. } | SceneOp::Join { .. }));
        if regrouped {
            self.invalidate();
        }
        let mut applied = applied(&scene.name, &households, report, notes);
        if let Some(over) = faded {
            let _ = write!(applied.done, " (volumes faded over {}s)", over.as_secs());
        }
        let decision = if applied.notes.is_empty() {
            "allow".to_string()
        } else {
            let clamps: Vec<&str> = applied.notes.iter().map(|n| n.detail.as_str()).collect();
            format!("clamp: {}", clamps.join("; "))
        };
        let mut logged = applied.done.clone();
        if !missed.is_empty() {
            let _ = write!(
                logged,
                " (before-state missing for {} zone(s): undo cannot fully restore)",
                missed.len()
            );
        }
        self.record(
            client,
            format!("{tool}: {}", scene.name),
            decision,
            logged,
            actions::before_state_with(&snaps, &sessions),
        );
        Ok(applied)
    }

    /// Authorize `tool` (a write), logging a refusal as every control call
    /// does.
    pub(super) fn authorize_logged(&self, client: &Client, tool: &str) -> Result<(), Failure> {
        self.guard(client)
            .authorize(tool, false)
            .inspect_err(|denied| {
                self.record(
                    client,
                    tool.to_string(),
                    format!("deny: {}", denied.detail),
                    denied.detail.clone(),
                    None,
                );
            })
    }

    /// `plan` with each volume bounded by the house policy for `client`: a
    /// level over a cap is lowered to it, and a raise the policy refuses is
    /// left out; each with a `VOLUME_CLAMPED` note.
    fn bound_volumes(
        &self,
        client: &Client,
        households: &[HouseholdState],
        plan: ScenePlan,
    ) -> (ScenePlan, Vec<Note>) {
        let guard = self.guard(client);
        let mut notes = Vec::new();
        let mut ops = Vec::with_capacity(plan.ops.len());
        for op in plan.ops {
            let (room, player, level) = match op {
                SceneOp::Volume {
                    room,
                    player,
                    level,
                } => (room, player, level),
                other => {
                    ops.push(other);
                    continue;
                }
            };
            let asked = Command::Volume {
                target: player.clone(),
                scope: VolumeScope::Room,
                change: VolumeChange::Set(level),
            };
            match guard.bound(&*self.transport, households, asked) {
                Ok((bounded, clamped)) => {
                    let level = match bounded {
                        Command::Volume {
                            change: VolumeChange::Set(allowed),
                            ..
                        } => allowed,
                        _ => level,
                    };
                    notes.extend(clamped);
                    ops.push(SceneOp::Volume {
                        room,
                        player,
                        level,
                    });
                }
                Err(refused) => notes.push(Note {
                    code: NoteCode::VolumeClamped,
                    detail: format!("{room}'s volume left as it is: {}", refused.detail),
                }),
            }
        }
        (
            ScenePlan {
                ops,
                notes: plan.notes,
            },
            notes,
        )
    }
}
