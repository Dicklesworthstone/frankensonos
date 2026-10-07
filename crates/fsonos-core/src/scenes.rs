//! Scenes: named house states to save and come back to.
//!
//! A [`Scene`] records, by room name, how the rooms group, each room's
//! volume and mute, and what each group plays and whether it is playing.
//! [`capture`] saves the house as it is, through the snapshot model.
//! Applying one takes three steps, so a surface can log and undo it like any
//! other action:
//!
//! 1. [`zones`] names the zones the scene touches, for the caller to
//!    snapshot (the action log's before-state);
//! 2. [`plan_apply`] diffs those snapshots against the scene and returns the
//!    fewest ordered ops that get there (none when the house already
//!    matches): regroup, then volumes and mutes, then sources, then play or
//!    pause. It is pure;
//! 3. [`apply`] runs the ops, fading volumes when asked, and carries on past
//!    an op that fails.
//!
//! Rooms are named, so a scene survives regrouping and new addresses; a room
//! that is gone (or renamed) is skipped with a note. Rooms in different
//! households can never share a group (S1 and S2 are always separate), so
//! [`check`] rejects such a scene. A group's queue is not part of a scene:
//! a group that played its queue keeps whatever it has
//! ([`SceneSource::Keep`]). The DJ is started through [`SceneHooks`].

use crate::fade::Fader;
use crate::favorites::{self, Favorite};
use crate::snapshot::{self, SnapshotSource, ZoneSnapshot};
use crate::store::{DjSession, Store, StoreError, StoredScene};
use crate::{CoreError, HouseholdState, Room, control};
use fsonos_proto::Transport;
use fsonos_types::{PlayerId, TransportState};
use serde::{Deserialize, Serialize};
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

/// The longest scene name.
pub const MAX_NAME: usize = 64;

/// A named house state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scene {
    pub name: String,
    pub groups: Vec<SceneGroup>,
    /// Room name → volume.
    #[serde(default)]
    pub volumes: BTreeMap<String, u8>,
    /// Room name → mute.
    #[serde(default)]
    pub mutes: BTreeMap<String, bool>,
}

/// One group of a scene (a room on its own is a group without members).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SceneGroup {
    /// The room that leads the group.
    pub coordinator: String,
    /// The other rooms in it.
    #[serde(default)]
    pub members: Vec<String>,
    #[serde(default)]
    pub source: SceneSource,
    /// Playing, or else paused (or stopped).
    #[serde(default)]
    pub playing: bool,
}

/// What a scene group plays.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SceneSource {
    /// Whatever the group has (its queue, say).
    #[default]
    Keep,
    /// A Sonos favorite, by title. `uri`, when known, tells whether it is
    /// already on.
    Favorite {
        name: String,
        #[serde(default)]
        uri: Option<String>,
    },
    /// A URI with its DIDL-Lite metadata.
    Uri {
        uri: String,
        #[serde(default)]
        metadata: String,
    },
    /// The DJ, in `mood`.
    Dj {
        #[serde(default)]
        mood: Option<String>,
    },
}

/// Why a scene cannot be saved, found, or applied.
#[derive(Debug, thiserror::Error)]
pub enum SceneError {
    #[error("a scene needs a name of 1 to {MAX_NAME} characters")]
    BadName,
    #[error("{} are in different households, which can never be grouped (S1 and S2 are always separate)", .rooms.join(" and "))]
    CrossHousehold { rooms: Vec<String> },
    #[error("room {0:?} is in more than one group of the scene")]
    RoomTwice(String),
    #[error("no scene named {name:?}; scenes: {}", names(.known))]
    Unknown { name: String, known: Vec<String> },
    #[error("scene {name:?} is unreadable: {why}")]
    Corrupt { name: String, why: String },
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Core(#[from] CoreError),
}

fn names(known: &[String]) -> String {
    if known.is_empty() {
        "none yet".into()
    } else {
        known.join(", ")
    }
}

/// One step of applying a scene.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SceneOp {
    /// The room leaves its group for one of its own.
    Leave {
        room: String,
        player: PlayerId,
    },
    /// The room joins the group `coordinator` leads.
    Join {
        room: String,
        player: PlayerId,
        coordinator: PlayerId,
    },
    Volume {
        room: String,
        player: PlayerId,
        level: u8,
    },
    Mute {
        room: String,
        player: PlayerId,
        mute: bool,
    },
    /// Play the favorite titled `name` (this starts playback).
    Favorite {
        coordinator: PlayerId,
        name: String,
    },
    /// Play `uri` (this starts playback).
    Uri {
        coordinator: PlayerId,
        uri: String,
        metadata: String,
    },
    /// Start (or steer) the DJ.
    Dj {
        coordinator: PlayerId,
        mood: Option<String>,
    },
    Play {
        coordinator: PlayerId,
    },
    Pause {
        coordinator: PlayerId,
    },
}

/// The ops that apply a scene, and what was left out, with why.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScenePlan {
    pub ops: Vec<SceneOp>,
    pub notes: Vec<String>,
}

/// What applying a scene did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SceneReport {
    pub done: Vec<SceneOp>,
    pub failed: Vec<(SceneOp, String)>,
    pub notes: Vec<String>,
}

impl SceneReport {
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.failed.is_empty()
    }
}

/// What a scene needs from outside the core.
pub trait SceneHooks {
    /// Start the DJ in the group `coordinator` leads, in `mood`, or steer
    /// the one already playing there.
    fn start_dj(&self, coordinator: &PlayerId, mood: Option<&str>) -> Result<(), String>;
}

/// For callers without a DJ: a DJ source fails, with why.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoHooks;

impl SceneHooks for NoHooks {
    fn start_dj(&self, _: &PlayerId, _: Option<&str>) -> Result<(), String> {
        Err("the DJ is not available here".into())
    }
}

/// How [`apply`] runs.
#[derive(Clone, Copy)]
pub struct ApplyOptions<'a> {
    /// Fade volumes over this long with this fader (else set them at once).
    pub fade: Option<(&'a Fader, Duration)>,
    pub hooks: &'a dyn SceneHooks,
}

impl Default for ApplyOptions<'_> {
    fn default() -> Self {
        Self {
            fade: None,
            hooks: &NoHooks,
        }
    }
}

/// The room called `name` (ignoring case), and its household.
fn find_room<'a>(
    households: &'a [HouseholdState],
    name: &str,
) -> Option<(usize, &'a HouseholdState, &'a Room)> {
    households.iter().enumerate().find_map(|(i, h)| {
        h.rooms
            .iter()
            .find(|r| r.name.eq_ignore_ascii_case(name.trim()))
            .map(|r| (i, h, r))
    })
}

/// Whether `room` leads the group it plays in.
fn leads(room: &Room) -> bool {
    room.players.contains(&room.coordinator)
}

fn check_name(name: &str) -> Result<(), SceneError> {
    let n = name.trim().chars().count();
    if n == 0 || n > MAX_NAME {
        return Err(SceneError::BadName);
    }
    Ok(())
}

/// Check `scene` against the house as it is: its name, that no room is in
/// two groups, and that each group's rooms share a household. Returns notes
/// for rooms that are gone.
pub fn check(scene: &Scene, households: &[HouseholdState]) -> Result<Vec<String>, SceneError> {
    check_name(&scene.name)?;
    let mut notes = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for g in &scene.groups {
        let mut house = None;
        let mut rooms = Vec::new();
        for name in std::iter::once(&g.coordinator).chain(&g.members) {
            let key = name.trim().to_lowercase();
            if seen.contains(&key) {
                return Err(SceneError::RoomTwice(name.clone()));
            }
            seen.push(key);
            match find_room(households, name) {
                None => notes.push(format!("{name} is not in the house now; skipped")),
                Some((i, _, r)) => {
                    rooms.push(r.name.clone());
                    if *house.get_or_insert(i) != i {
                        return Err(SceneError::CrossHousehold { rooms });
                    }
                }
            }
        }
    }
    for name in scene.volumes.keys().chain(scene.mutes.keys()) {
        let note = format!("{name} is not in the house now; skipped");
        if find_room(households, name).is_none() && !notes.contains(&note) {
            notes.push(note);
        }
    }
    Ok(notes)
}

/// The zones (coordinators) applying `scene` may change: snapshot these
/// first.
#[must_use]
pub fn zones(scene: &Scene, households: &[HouseholdState]) -> Vec<PlayerId> {
    let mut zones: Vec<PlayerId> = Vec::new();
    let named = scene
        .groups
        .iter()
        .flat_map(|g| std::iter::once(&g.coordinator).chain(&g.members))
        .chain(scene.volumes.keys())
        .chain(scene.mutes.keys());
    for name in named {
        if let Some((_, _, r)) = find_room(households, name)
            && !zones.contains(&r.coordinator)
        {
            zones.push(r.coordinator.clone());
        }
    }
    zones
}

/// The fewest ordered ops that take the house from `snaps` (snapshots of
/// [`zones`]) to `scene`.
pub fn plan_apply(
    scene: &Scene,
    households: &[HouseholdState],
    snaps: &[ZoneSnapshot],
) -> Result<ScenePlan, SceneError> {
    let mut plan = ScenePlan {
        ops: Vec::new(),
        notes: check(scene, households)?,
    };
    plan_groups(scene, households, &mut plan);
    plan_levels(scene, households, snaps, &mut plan);
    plan_sources(scene, households, snaps, &mut plan);
    Ok(plan)
}

/// Leaves first (each scene group's lead must lead), then joins.
fn plan_groups(scene: &Scene, households: &[HouseholdState], plan: &mut ScenePlan) {
    let mut joins = Vec::new();
    for g in &scene.groups {
        let Some((_, _, lead)) = find_room(households, &g.coordinator) else {
            continue;
        };
        if !leads(lead) {
            plan.ops.push(SceneOp::Leave {
                room: lead.name.clone(),
                player: lead.primary.clone(),
            });
        }
        for name in &g.members {
            let Some((_, _, room)) = find_room(households, name) else {
                continue;
            };
            let with_lead = leads(lead) && lead.players.contains(&room.coordinator);
            if !with_lead {
                joins.push(SceneOp::Join {
                    room: room.name.clone(),
                    player: room.primary.clone(),
                    coordinator: lead.primary.clone(),
                });
            }
        }
    }
    plan.ops.extend(joins);
}

/// Volumes and mutes that differ from the snapshots (or are unknown).
fn plan_levels(
    scene: &Scene,
    households: &[HouseholdState],
    snaps: &[ZoneSnapshot],
    plan: &mut ScenePlan,
) {
    let level = |p: &PlayerId| {
        snaps
            .iter()
            .flat_map(|s| &s.levels)
            .find(|l| l.player == *p)
    };
    for (name, &want) in &scene.volumes {
        if let Some((_, _, r)) = find_room(households, name)
            && level(&r.primary).is_none_or(|l| l.volume != want)
        {
            plan.ops.push(SceneOp::Volume {
                room: r.name.clone(),
                player: r.primary.clone(),
                level: want,
            });
        }
    }
    for (name, &want) in &scene.mutes {
        if let Some((_, _, r)) = find_room(households, name)
            && level(&r.primary).is_none_or(|l| l.mute != want)
        {
            plan.ops.push(SceneOp::Mute {
                room: r.name.clone(),
                player: r.primary.clone(),
                mute: want,
            });
        }
    }
}

/// Each group's source, unless it is already on, then play or pause.
fn plan_sources(
    scene: &Scene,
    households: &[HouseholdState],
    snaps: &[ZoneSnapshot],
    plan: &mut ScenePlan,
) {
    for g in &scene.groups {
        let Some((_, _, lead)) = find_room(households, &g.coordinator) else {
            continue;
        };
        // What the lead's group plays now; unknown if it does not lead yet.
        let now = snaps
            .iter()
            .find(|s| leads(lead) && lead.players.contains(&s.coordinator));
        let on_now = now.and_then(|s| match &s.source {
            SnapshotSource::Uri { uri, .. } => Some(uri.as_str()),
            _ => None,
        });
        let coordinator = lead.primary.clone();
        let source = match &g.source {
            SceneSource::Keep => None,
            SceneSource::Favorite { name, uri } => (uri.is_none() || on_now != uri.as_deref())
                .then(|| SceneOp::Favorite {
                    coordinator: coordinator.clone(),
                    name: name.clone(),
                }),
            SceneSource::Uri { uri, metadata } => {
                (on_now != Some(uri.as_str())).then(|| SceneOp::Uri {
                    coordinator: coordinator.clone(),
                    uri: uri.clone(),
                    metadata: metadata.clone(),
                })
            }
            SceneSource::Dj { mood } => Some(SceneOp::Dj {
                coordinator: coordinator.clone(),
                mood: mood.clone(),
            }),
        };
        let starts = source.is_some();
        let playing = !starts && now.is_some_and(|s| s.transport_state == TransportState::Playing);
        plan.ops.extend(source);
        if g.playing && !starts && !playing {
            plan.ops.push(SceneOp::Play { coordinator });
        } else if !g.playing && (starts || playing) {
            plan.ops.push(SceneOp::Pause { coordinator });
        }
    }
}

/// Run `plan`, carrying on past an op that fails.
pub fn apply<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    plan: &ScenePlan,
    options: &ApplyOptions<'_>,
) -> SceneReport {
    let mut report = SceneReport {
        notes: plan.notes.clone(),
        ..SceneReport::default()
    };
    let mut favorites: HashMap<usize, Vec<Favorite>> = HashMap::new();
    let mut ops = plan.ops.iter().peekable();
    while let Some(op) = ops.next() {
        // Faded volumes go together: every room heading for one level at once.
        if let (SceneOp::Volume { .. }, Some((fader, over))) = (op, options.fade) {
            let mut batch = vec![op];
            while let Some(next) = ops.next_if(|o| matches!(o, SceneOp::Volume { .. })) {
                batch.push(next);
            }
            fade_volumes(t, households, &batch, fader, over, &mut report);
            continue;
        }
        match run(t, households, op, options, &mut favorites) {
            Ok(()) => report.done.push(op.clone()),
            Err(e) => report.failed.push((op.clone(), e)),
        }
    }
    report
}

fn fade_volumes<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    batch: &[&SceneOp],
    fader: &Fader,
    over: Duration,
    report: &mut SceneReport,
) {
    let mut by_level: BTreeMap<u8, Vec<(&SceneOp, PlayerId)>> = BTreeMap::new();
    for op in batch {
        if let SceneOp::Volume { player, level, .. } = op {
            by_level
                .entry(*level)
                .or_default()
                .push((op, player.clone()));
        }
    }
    for (level, ops) in by_level {
        let players: Vec<PlayerId> = ops.iter().map(|(_, p)| p.clone()).collect();
        let result = fader.fade_group(t, households, &players, level, over);
        for (op, _) in ops {
            match &result {
                Ok(_) => report.done.push(op.clone()),
                Err(e) => report.failed.push((op.clone(), e.to_string())),
            }
        }
    }
}

fn run<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    op: &SceneOp,
    options: &ApplyOptions<'_>,
    favorites: &mut HashMap<usize, Vec<Favorite>>,
) -> Result<(), String> {
    let s = |e: CoreError| e.to_string();
    match op {
        SceneOp::Leave { player, .. } => control::leave(t, households, player).map_err(s),
        SceneOp::Join {
            player,
            coordinator,
            ..
        } => control::join(t, households, player, coordinator).map_err(s),
        SceneOp::Volume { player, level, .. } => control::set_volume(t, households, player, *level)
            .map(|_| ())
            .map_err(s),
        SceneOp::Mute { player, mute, .. } => {
            control::set_mute(t, households, player, *mute).map_err(s)
        }
        SceneOp::Favorite { coordinator, name } => {
            let house = households
                .iter()
                .position(|h| h.player(coordinator).is_some())
                .ok_or_else(|| format!("unknown player {}", coordinator.0))?;
            let list = match favorites.entry(house) {
                Entry::Occupied(known) => known.into_mut(),
                Entry::Vacant(slot) => {
                    slot.insert(favorites::list(t, households, coordinator).map_err(s)?)
                }
            };
            let favorite = match list.iter().find(|f| f.title == *name) {
                Some(f) => f,
                None => favorites::find(list, name).map_err(|e| e.to_string())?,
            };
            favorites::play(t, households, coordinator, favorite).map_err(|e| e.to_string())
        }
        SceneOp::Uri {
            coordinator,
            uri,
            metadata,
        } => control::play_uri(t, households, coordinator, uri, metadata).map_err(s),
        SceneOp::Dj { coordinator, mood } => options.hooks.start_dj(coordinator, mood.as_deref()),
        SceneOp::Play { coordinator } => control::resume(t, households, coordinator).map_err(s),
        // A stream may not pause; then it stops.
        SceneOp::Pause { coordinator } => control::pause(t, households, coordinator)
            .or_else(|_| control::stop(t, households, coordinator))
            .map_err(s),
    }
}

/// The house as it is now, as a scene called `name`: every group, every
/// room's level, and what each group plays (a live DJ session, a favorite
/// when the URI is one, a URI, or else whatever it has).
pub fn capture<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    name: &str,
    dj_sessions: &[DjSession],
    now: i64,
) -> Result<Scene, SceneError> {
    check_name(name)?;
    let mut scene = Scene {
        name: name.trim().to_string(),
        groups: Vec::new(),
        volumes: BTreeMap::new(),
        mutes: BTreeMap::new(),
    };
    for household in households {
        // Favorites are only a nicer name for a URI: carry on without them.
        let favorites = household
            .groups
            .first()
            .and_then(|g| favorites::list(t, households, &g.coordinator).ok())
            .unwrap_or_default();
        for group in &household.groups {
            let Some(lead) = household
                .rooms
                .iter()
                .find(|r| r.players.contains(&group.coordinator))
            else {
                continue;
            };
            let snap = snapshot::capture(t, households, &group.coordinator, now)?;
            for level in &snap.levels {
                if let Some(r) = household.rooms.iter().find(|r| r.primary == level.player) {
                    scene.volumes.insert(r.name.clone(), level.volume);
                    scene.mutes.insert(r.name.clone(), level.mute);
                }
            }
            let dj = dj_sessions
                .iter()
                .find(|d| d.coordinator == group.coordinator.0 && d.expires > now);
            let source = match (dj, &snap.source) {
                (Some(d), _) => SceneSource::Dj {
                    mood: d.mood.clone(),
                },
                (None, SnapshotSource::Uri { uri, metadata, .. }) => {
                    match favorites
                        .iter()
                        .find(|f| f.uri.as_deref() == Some(uri.as_str()))
                    {
                        Some(f) => SceneSource::Favorite {
                            name: f.title.clone(),
                            uri: Some(uri.clone()),
                        },
                        None => SceneSource::Uri {
                            uri: uri.clone(),
                            metadata: metadata.clone(),
                        },
                    }
                }
                (None, _) => SceneSource::Keep,
            };
            scene.groups.push(SceneGroup {
                coordinator: lead.name.clone(),
                members: household
                    .rooms
                    .iter()
                    .filter(|r| r.coordinator == group.coordinator && r.primary != lead.primary)
                    .map(|r| r.name.clone())
                    .collect(),
                source,
                playing: snap.transport_state == TransportState::Playing,
            });
        }
    }
    Ok(scene)
}

/// Store `scene` (replacing one of the same name) as of `now`.
pub fn save<S: Store + ?Sized>(store: &mut S, scene: &Scene, now: i64) -> Result<(), SceneError> {
    check_name(&scene.name)?;
    check(scene, &[])?;
    let spec = serde_json::to_string(scene).map_err(|e| SceneError::Corrupt {
        name: scene.name.clone(),
        why: e.to_string(),
    })?;
    Ok(store.save_scene(&StoredScene {
        name: scene.name.trim().to_string(),
        spec,
        updated: now,
    })?)
}

fn parse(stored: &StoredScene) -> Result<Scene, SceneError> {
    serde_json::from_str(&stored.spec).map_err(|e| SceneError::Corrupt {
        name: stored.name.clone(),
        why: e.to_string(),
    })
}

/// The stored scene called `name` (ignoring case).
fn stored<S: Store + ?Sized>(store: &S, name: &str) -> Result<StoredScene, SceneError> {
    if let Some(s) = store.scene(name.trim())? {
        return Ok(s);
    }
    let all = store.scenes()?;
    let known = all.iter().map(|s| s.name.clone()).collect();
    all.into_iter()
        .find(|s| s.name.eq_ignore_ascii_case(name.trim()))
        .ok_or(SceneError::Unknown {
            name: name.to_string(),
            known,
        })
}

/// The scene called `name` (ignoring case).
pub fn load<S: Store + ?Sized>(store: &S, name: &str) -> Result<Scene, SceneError> {
    parse(&stored(store, name)?)
}

/// Every scene, by name, with when it was last saved.
pub fn list<S: Store + ?Sized>(store: &S) -> Result<Vec<(Scene, i64)>, SceneError> {
    store
        .scenes()?
        .iter()
        .map(|s| Ok((parse(s)?, s.updated)))
        .collect()
}

/// Forget the scene called `name` (ignoring case).
pub fn delete<S: Store + ?Sized>(store: &mut S, name: &str) -> Result<(), SceneError> {
    let found = stored(store, name)?;
    store.delete_scene(&found.name)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::MemberLevel;
    use crate::store::MemStore;
    use fsonos_types::ZoneGroup;

    fn pid(s: &str) -> PlayerId {
        PlayerId(s.into())
    }

    /// One household per entry; rooms are named after their (single)
    /// players and grouped as listed, coordinator first.
    fn house(households: &[&[&[&str]]]) -> Vec<HouseholdState> {
        households
            .iter()
            .map(|groups| {
                let mut h = HouseholdState::default();
                for g in *groups {
                    h.groups.push(ZoneGroup {
                        coordinator: pid(g[0]),
                        members: g.iter().map(|m| pid(m)).collect(),
                    });
                    for m in *g {
                        h.rooms.push(Room {
                            name: (*m).into(),
                            primary: pid(m),
                            players: vec![pid(m)],
                            missing: Vec::new(),
                            coordinator: pid(g[0]),
                        });
                    }
                }
                h
            })
            .collect()
    }

    fn group(lead: &str, members: &[&str], source: SceneSource, playing: bool) -> SceneGroup {
        SceneGroup {
            coordinator: lead.into(),
            members: members.iter().map(|m| (*m).into()).collect(),
            source,
            playing,
        }
    }

    fn snap(
        lead: &str,
        members: &[(&str, u8, bool)],
        state: TransportState,
        source: SnapshotSource,
    ) -> ZoneSnapshot {
        ZoneSnapshot {
            household: None,
            coordinator: pid(lead),
            members: members.iter().map(|m| pid(m.0)).collect(),
            levels: members
                .iter()
                .map(|&(p, volume, mute)| MemberLevel {
                    player: pid(p),
                    volume,
                    mute,
                })
                .collect(),
            group_volume: None,
            transport_state: state,
            source,
            captured_at: 0,
        }
    }

    fn radio() -> SnapshotSource {
        SnapshotSource::Uri {
            uri: "x-rincon-mp3radio://radio.example/a".into(),
            metadata: String::new(),
            position_secs: None,
        }
    }

    /// The house of `house(&[&[&["A", "B"], &["C"]]])` playing radio in A+B
    /// and nothing in C, as a scene.
    fn evening() -> Scene {
        Scene {
            name: "Evening".into(),
            groups: vec![
                group(
                    "A",
                    &["B"],
                    SceneSource::Favorite {
                        name: "Radio A".into(),
                        uri: Some("x-rincon-mp3radio://radio.example/a".into()),
                    },
                    true,
                ),
                group("C", &[], SceneSource::Keep, false),
            ],
            volumes: [("A", 30), ("B", 20), ("C", 10)]
                .iter()
                .map(|(r, v)| ((*r).to_string(), *v))
                .collect(),
            mutes: [("A", false), ("B", false), ("C", true)]
                .iter()
                .map(|(r, m)| ((*r).to_string(), *m))
                .collect(),
        }
    }

    fn evening_snaps() -> Vec<ZoneSnapshot> {
        vec![
            snap(
                "A",
                &[("A", 30, false), ("B", 20, false)],
                TransportState::Playing,
                radio(),
            ),
            snap(
                "C",
                &[("C", 10, true)],
                TransportState::Stopped,
                SnapshotSource::Nothing,
            ),
        ]
    }

    #[test]
    fn nothing_to_do_when_the_house_is_already_in_the_scene() {
        let h = house(&[&[&["A", "B"], &["C"]]]);
        let plan = plan_apply(&evening(), &h, &evening_snaps()).unwrap();
        assert_eq!(plan, ScenePlan::default());
        assert_eq!(zones(&evening(), &h), [pid("A"), pid("C")]);
    }

    #[test]
    fn only_what_differs_is_changed_in_order() {
        // C joined A's group, B is standalone and louder, A is paused on
        // another station.
        let h = house(&[&[&["A", "C"], &["B"]]]);
        let snaps = vec![
            snap(
                "A",
                &[("A", 30, false), ("C", 10, true)],
                TransportState::Paused,
                SnapshotSource::Uri {
                    uri: "x-rincon-mp3radio://radio.example/b".into(),
                    metadata: String::new(),
                    position_secs: None,
                },
            ),
            snap(
                "B",
                &[("B", 45, false)],
                TransportState::Stopped,
                SnapshotSource::Nothing,
            ),
        ];
        let plan = plan_apply(&evening(), &h, &snaps).unwrap();
        assert_eq!(
            plan.ops,
            [
                SceneOp::Leave {
                    room: "C".into(),
                    player: pid("C"),
                },
                SceneOp::Join {
                    room: "B".into(),
                    player: pid("B"),
                    coordinator: pid("A"),
                },
                SceneOp::Volume {
                    room: "B".into(),
                    player: pid("B"),
                    level: 20,
                },
                SceneOp::Favorite {
                    coordinator: pid("A"),
                    name: "Radio A".into(),
                },
            ]
        );
        assert_eq!(zones(&evening(), &h), [pid("A"), pid("B")]);

        // Same station, but stopped: just play. A playing C: just pause.
        let h = house(&[&[&["A", "B"], &["C"]]]);
        let mut snaps = evening_snaps();
        snaps[0].transport_state = TransportState::Stopped;
        snaps[1].transport_state = TransportState::Playing;
        let plan = plan_apply(&evening(), &h, &snaps).unwrap();
        assert_eq!(
            plan.ops,
            [
                SceneOp::Play {
                    coordinator: pid("A"),
                },
                SceneOp::Pause {
                    coordinator: pid("C"),
                },
            ]
        );
    }

    #[test]
    fn a_lead_that_is_a_member_leaves_first_and_its_rooms_follow() {
        // The scene: B leads C. Now: A leads B and C.
        let h = house(&[&[&["A", "B", "C"]]]);
        let scene = Scene {
            name: "B leads".into(),
            groups: vec![group("B", &["C"], SceneSource::Keep, false)],
            volumes: BTreeMap::new(),
            mutes: BTreeMap::new(),
        };
        let snaps = [snap(
            "A",
            &[("A", 1, false)],
            TransportState::Stopped,
            SnapshotSource::Nothing,
        )];
        let plan = plan_apply(&scene, &h, &snaps).unwrap();
        assert_eq!(
            plan.ops,
            [
                SceneOp::Leave {
                    room: "B".into(),
                    player: pid("B"),
                },
                SceneOp::Join {
                    room: "C".into(),
                    player: pid("C"),
                    coordinator: pid("B"),
                },
            ]
        );
    }

    #[test]
    fn rooms_of_two_households_never_share_a_group() {
        let h = house(&[&[&["A"]], &[&["X"]]]);
        let mut scene = evening();
        scene.groups = vec![group("A", &["X"], SceneSource::Keep, true)];
        let err = plan_apply(&scene, &h, &[]).unwrap_err();
        assert!(matches!(&err, SceneError::CrossHousehold { rooms } if rooms == &["A", "X"]));
        assert_eq!(
            err.to_string(),
            "A and X are in different households, which can never be grouped (S1 and S2 are always separate)"
        );
        scene.groups = vec![
            group("A", &[], SceneSource::Keep, true),
            group("a", &[], SceneSource::Keep, true),
        ];
        assert!(matches!(plan_apply(&scene, &h, &[]), Err(SceneError::RoomTwice(r)) if r == "a"));
    }

    #[test]
    fn rooms_that_are_gone_are_skipped_with_a_note() {
        let h = house(&[&[&["A"]]]);
        let mut scene = evening();
        scene.groups = vec![group("A", &["Attic"], SceneSource::Keep, false)];
        scene.volumes = [("Attic".to_string(), 5), ("a".to_string(), 7)].into();
        scene.mutes.clear();
        let plan = plan_apply(&scene, &h, &[]).unwrap();
        assert_eq!(plan.notes, ["Attic is not in the house now; skipped"]);
        assert_eq!(
            plan.ops,
            [SceneOp::Volume {
                room: "A".into(),
                player: pid("A"),
                level: 7,
            }],
            "names match ignoring case; an unknown level is always set"
        );
    }

    #[test]
    fn scenes_round_trip_through_the_store() {
        let mut store = MemStore::default();
        assert!(matches!(
            load(&store, "evening"),
            Err(SceneError::Unknown { .. })
        ));
        save(&mut store, &evening(), 100).unwrap();
        assert_eq!(load(&store, "  EVENING ").unwrap(), evening());
        let mut later = evening();
        later.volumes.insert("A".into(), 50);
        save(&mut store, &later, 200).unwrap();
        assert_eq!(list(&store).unwrap(), [(later, 200)]);
        let json = store.scene("Evening").unwrap().unwrap().spec;
        assert!(json.contains("\"kind\":\"favorite\""), "{json}");

        let mut bad = evening();
        bad.name = " ".into();
        assert!(matches!(
            save(&mut store, &bad, 1),
            Err(SceneError::BadName)
        ));
        delete(&mut store, "evening").unwrap();
        let err = delete(&mut store, "evening").unwrap_err();
        assert_eq!(
            err.to_string(),
            "no scene named \"evening\"; scenes: none yet"
        );

        // Hand-written specs need only the essentials.
        let minimal: Scene =
            serde_json::from_str(r#"{"name":"Quiet","groups":[{"coordinator":"Den"}]}"#).unwrap();
        assert_eq!(minimal.groups[0].source, SceneSource::Keep);
        assert!(!minimal.groups[0].playing);
    }
}
