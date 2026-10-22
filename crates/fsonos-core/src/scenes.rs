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
//! 3. [`apply`] confirms grouping before playback and isolates failed
//!    dependencies while continuing independent groups. Asked to
//!    fade, it leaves the volumes for last and fades them together, and a
//!    room whose sound starts with the scene fades in from silence.
//!
//! Rooms are named, so a scene survives regrouping and new addresses; a room
//! that is gone (or renamed) is skipped with a note. Household-qualified
//! names and player ids disambiguate duplicate names; ambiguity is an error,
//! never a first-match choice. Captures retain every duplicate room. Rooms
//! in different households can never share a group (S1 and S2 are always separate), so
//! [`check`] rejects such a scene. A captured queue source retains a guarded
//! bookmark into the original queue: its occurrence, position and known play
//! mode. Recall refuses an edited queue or a different coordinator; it never
//! clears or rebuilds queue contents. The DJ is started through [`SceneHooks`].

use crate::dj_direction::DjDirection;
use crate::fade::Fader;
use crate::favorites::{self, Favorite};
use crate::rooms::{named_room, normalize_room};
use crate::snapshot::{self, SnapshotSource, ZoneSnapshot};
use crate::store::{Store, StoreError, StoredScene};
use crate::{CoreError, HouseholdState, Room, control};
use fsonos_proto::Transport;
use fsonos_types::{PlayerId, TransportState};
use serde::{Deserialize, Serialize};
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

mod execution;
mod layout;
mod queue;
mod targets;

pub use queue::QueueReference;
pub use targets::canonical_room_name;

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
    /// Who saved it, by policy key (`cli`, a tailnet login, ...): applying
    /// it never sets a volume higher than that client may. `None` in a
    /// scene saved before writers were kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writer: Option<String>,
}

/// One group of a scene (a room on its own is a group without members).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SceneGroup {
    /// The room that leads the group.
    pub coordinator: String,
    /// Exactly the other rooms in it. Unlisted rooms currently in this
    /// group are detached; an empty list makes the coordinator's room solo.
    #[serde(default)]
    pub members: Vec<String>,
    #[serde(default)]
    pub source: SceneSource,
    /// Playing, or else paused (or stopped); `None` leaves the group's
    /// playback as it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playing: Option<bool>,
}

/// What a scene group plays.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)] // one declarative source per saved room group
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
    /// A URI with its DIDL-Lite metadata, or a Spotify track, album or
    /// playlist (`spotify:<kind>:<id>`), played through the household's own
    /// Spotify account.
    Uri {
        uri: String,
        #[serde(default)]
        metadata: String,
    },
    /// A captured occurrence in an unchanged queue on its original player.
    Queue { queue: QueueReference },
    /// The DJ, in `mood`, with an optional reusable steering request.
    Dj {
        #[serde(default)]
        mood: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        direction: Option<DjDirection>,
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
#[allow(clippy::large_enum_variant)] // bounded scene plan, preserving the typed direction
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
    /// Play the favorite titled `name` (this starts playback), or with
    /// `start` false load it paused, ready to resume, without a sound.
    Favorite {
        coordinator: PlayerId,
        name: String,
        start: bool,
    },
    /// Play `uri` (this starts playback), or with `start` false load it
    /// paused, ready to resume, without a sound.
    Uri {
        coordinator: PlayerId,
        uri: String,
        metadata: String,
        start: bool,
    },
    /// Recall a guarded queue bookmark. `start` is the final playback
    /// intent: false loads it silently, ready to resume.
    Queue {
        coordinator: PlayerId,
        queue: QueueReference,
        start: bool,
    },
    /// Start (or steer) the DJ.
    Dj {
        coordinator: PlayerId,
        mood: Option<String>,
        direction: Option<DjDirection>,
    },
    Play {
        coordinator: PlayerId,
    },
    Pause {
        coordinator: PlayerId,
    },
}

impl SceneOp {
    /// Whether this step starts the group's sound: a Play, the DJ, or a
    /// source played rather than loaded.
    #[must_use]
    pub fn starts(&self) -> bool {
        match self {
            Self::Favorite { start, .. } | Self::Uri { start, .. } | Self::Queue { start, .. } => {
                *start
            }
            Self::Dj { .. } | Self::Play { .. } => true,
            Self::Leave { .. }
            | Self::Join { .. }
            | Self::Volume { .. }
            | Self::Mute { .. }
            | Self::Pause { .. } => false,
        }
    }
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
    /// How many rooms a fade carried to their level.
    pub faded: usize,
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

    /// Restore an explicit direction before starting the DJ. Older hooks
    /// continue to support legacy sources, but cannot silently discard a
    /// saved direction (including an explicit return to automatic mode).
    fn start_dj_with_direction(
        &self,
        coordinator: &PlayerId,
        mood: Option<&str>,
        direction: Option<&DjDirection>,
    ) -> Result<(), String> {
        if direction.is_some() {
            return Err("this DJ hook cannot restore a saved direction".into());
        }
        self.start_dj(coordinator, mood)
    }
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

/// A uniquely named room and its household. Planning first runs `check`,
/// which propagates ambiguity; read-only zone enumeration never guesses.
fn find_room<'a>(
    households: &'a [HouseholdState],
    name: &str,
) -> Option<(usize, &'a HouseholdState, &'a Room)> {
    named_room(households, name)
        .ok()
        .flatten()
        .map(|(i, room)| (i, &households[i], room))
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
    let mut players: Vec<(usize, PlayerId)> = Vec::new();
    for g in &scene.groups {
        if let SceneSource::Queue { queue } = &g.source {
            queue.validate().map_err(|why| SceneError::Corrupt {
                name: scene.name.clone(),
                why,
            })?;
        }
        let mut house = None;
        let mut rooms = Vec::new();
        for name in std::iter::once(&g.coordinator).chain(&g.members) {
            let key = normalize_room(name);
            if seen.contains(&key) {
                return Err(SceneError::RoomTwice(name.clone()));
            }
            seen.push(key);
            match named_room(households, name)? {
                None => notes.push(format!("{name} is not in the house now; skipped")),
                Some((i, r)) => {
                    let identity = (i, r.primary.clone());
                    if players.contains(&identity) {
                        return Err(SceneError::RoomTwice(name.clone()));
                    }
                    players.push(identity);
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
        if named_room(households, name)?.is_none() && !notes.contains(&note) {
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

/// Establish each declared group's exact membership before changing sound.
fn plan_groups(scene: &Scene, households: &[HouseholdState], plan: &mut ScenePlan) {
    layout::plan(scene, households, plan);
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
        let playing_now = now.is_some_and(|s| s.transport_state == TransportState::Playing);
        if let SceneSource::Queue { queue } = &g.source {
            // Even an apparently unchanged bookmark needs a live reboot and
            // revision check. Execution reports a verified no-op separately.
            plan.ops.push(SceneOp::Queue {
                coordinator,
                queue: queue.clone(),
                start: g.playing.unwrap_or(playing_now),
            });
            continue;
        }
        // A group that leads but whose state was not read may be playing:
        // its source takes the old path. One that does not lead yet will
        // leave its group, which stops it.
        let maybe_playing = playing_now || (now.is_none() && leads(lead));
        // A silent group the scene wants paused gets its source loaded
        // without a sound: played and then paused, it would sound for a
        // moment. One that plays now is played and then paused, as ever, so
        // a source that fails leaves its music playing as it was.
        let start = g.playing != Some(false) || maybe_playing;
        let source = match &g.source {
            SceneSource::Keep | SceneSource::Queue { .. } => None,
            SceneSource::Favorite { name, uri } => (uri.is_none() || on_now != uri.as_deref())
                .then(|| SceneOp::Favorite {
                    coordinator: coordinator.clone(),
                    name: name.clone(),
                    start,
                }),
            SceneSource::Uri { uri, metadata } => {
                // A Spotify track renders as x-sonos-spotify: the same track.
                let same = on_now == Some(uri.as_str())
                    || on_now
                        .and_then(fsonos_proto::didl::spotify_uri_from_renderer_uri)
                        .as_deref()
                        == Some(uri.as_str());
                (!same).then(|| SceneOp::Uri {
                    coordinator: coordinator.clone(),
                    uri: uri.clone(),
                    metadata: metadata.clone(),
                    start,
                })
            }
            SceneSource::Dj { mood, direction } => Some(SceneOp::Dj {
                coordinator: coordinator.clone(),
                mood: mood.clone(),
                direction: direction.clone(),
            }),
        };
        let starts = source.as_ref().is_some_and(SceneOp::starts);
        let playing = source.is_none() && playing_now;
        plan.ops.extend(source);
        match g.playing {
            Some(true) if !starts && !playing => plan.ops.push(SceneOp::Play { coordinator }),
            Some(false) if starts || playing => plan.ops.push(SceneOp::Pause { coordinator }),
            // As asked already, or (`None`) left as it is.
            _ => {}
        }
    }
}

/// Run `plan`, confirming group transitions before dependent sound changes.
/// Failed dependencies are skipped; independent groups continue. With a fade, volumes
/// go last and together, each room to its own level over the one fade, and a
/// room whose sound starts with the scene fades in from silence.
pub fn apply<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    plan: &ScenePlan,
    options: &ApplyOptions<'_>,
) -> SceneReport {
    execution::apply(t, households, plan, options)
}

/// Before a faded scene's sources start: silence the rooms whose sound
/// starts with it, so they fade in rather than start at their full level.
/// Those are the rooms of a group the plan starts (a Play, or a source for a
/// group that isn't playing; one that plays keeps sounding while its source
/// or the DJ's steering changes) and the rooms it unmutes. Each is returned
/// with the level to fade in to: the plan's, or the one it had.
fn silence_starting<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    plan: &ScenePlan,
    fader: &Fader,
) -> Vec<(PlayerId, u8)> {
    let rooms = || households.iter().flat_map(|h| &h.rooms);
    let mut starts: Vec<&PlayerId> = Vec::new();
    for op in &plan.ops {
        match op {
            SceneOp::Play { coordinator } => starts.push(coordinator),
            SceneOp::Favorite {
                coordinator,
                start: true,
                ..
            }
            | SceneOp::Uri {
                coordinator,
                start: true,
                ..
            }
            | SceneOp::Queue {
                coordinator,
                start: true,
                ..
            }
            | SceneOp::Dj { coordinator, .. } => {
                let idle = control::playback(t, households, coordinator)
                    .is_ok_and(|now| now.transport.state != TransportState::Playing);
                if idle {
                    starts.push(coordinator);
                }
            }
            _ => {}
        }
    }
    // The room that leads `room` once the plan has regrouped.
    let leader = |room: &Room| -> PlayerId {
        let moved = plan.ops.iter().rev().find_map(|op| match op {
            SceneOp::Join {
                player,
                coordinator,
                ..
            } if *player == room.primary => Some(coordinator.clone()),
            SceneOp::Leave { player, .. } if *player == room.primary => Some(player.clone()),
            _ => None,
        });
        let lead = moved.unwrap_or_else(|| room.coordinator.clone());
        rooms()
            .find(|r| r.players.contains(&lead))
            .map_or_else(|| lead.clone(), |r| r.primary.clone())
    };
    let wanted = |player: &PlayerId| {
        plan.ops.iter().find_map(|op| match op {
            SceneOp::Volume {
                player: p, level, ..
            } if p == player => Some(*level),
            _ => None,
        })
    };
    let unmuted = |player: &PlayerId| {
        plan.ops
            .iter()
            .any(|op| matches!(op, SceneOp::Mute { player: p, mute: false, .. } if p == player))
    };
    let mut ins = Vec::new();
    for room in rooms() {
        if !starts.contains(&&leader(room)) && !unmuted(&room.primary) {
            continue;
        }
        let level =
            wanted(&room.primary).or_else(|| control::volume(t, households, &room.primary).ok());
        let Some(to) = level else {
            continue;
        };
        if to > 0 && fader.set_volume(t, households, &room.primary, 0).is_ok() {
            ins.push((room.primary.clone(), to));
        }
    }
    ins
}

/// A faded scene's volumes: every room the plan sets and `ins` (the rooms
/// silenced to fade in), all together, each to its own level. If the fade
/// can't run (a player that doesn't answer), each level is set at once
/// instead, so no room is left part-way.
fn fade_volumes<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    plan: &ScenePlan,
    ins: &[(PlayerId, u8)],
    (fader, over): (&Fader, Duration),
    report: &mut SceneReport,
) {
    let ops: Vec<&SceneOp> = plan
        .ops
        .iter()
        .filter(|op| matches!(op, SceneOp::Volume { .. }))
        .collect();
    let mut targets: Vec<(PlayerId, u8)> = ops
        .iter()
        .filter_map(|op| match op {
            SceneOp::Volume { player, level, .. } => Some((player.clone(), *level)),
            _ => None,
        })
        .collect();
    // A room silenced to fade in that the plan sets no volume for goes back
    // to the level it had.
    for (player, to) in ins {
        if !targets.iter().any(|(p, _)| p == player) {
            targets.push((player.clone(), *to));
        }
    }
    if targets.is_empty() {
        return;
    }
    match fader.fade_each(t, households, &targets, over) {
        Ok(left) => {
            report.faded = targets.len().saturating_sub(left.len());
            report.done.extend(ops.into_iter().cloned());
            for player in &left {
                let room = households
                    .iter()
                    .flat_map(|h| &h.rooms)
                    .find(|r| r.players.contains(player))
                    .map_or(player.0.as_str(), |r| r.name.as_str());
                report.notes.push(format!(
                    "{room}'s fade stopped where its volume was changed meanwhile"
                ));
            }
        }
        Err(_) => {
            for (player, to) in &targets {
                let set = control::set_volume(t, households, player, *to);
                let op = ops
                    .iter()
                    .find(|op| matches!(op, SceneOp::Volume { player: p, .. } if p == player));
                match (op, set) {
                    (Some(op), Ok(_)) => report.done.push(SceneOp::clone(op)),
                    (Some(op), Err(e)) => report.failed.push((SceneOp::clone(op), e.to_string())),
                    // A room silenced to fade in, still silent: say so.
                    (None, Err(e)) => {
                        let room = households
                            .iter()
                            .flat_map(|h| &h.rooms)
                            .find(|r| r.players.contains(player))
                            .map_or(player.0.as_str(), |r| r.name.as_str());
                        report.notes.push(format!(
                            "{room} was silenced for the fade and couldn't be put back: {e}"
                        ));
                    }
                    (None, Ok(_)) => {}
                }
            }
        }
    }
}

/// Start `uri` on the group `coordinator` leads: a Spotify track, album
/// or playlist through the household's own Spotify account (as `play`
/// does), anything else as the renderer URI it is.
fn play_source<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    uri: &str,
    metadata: &str,
) -> Result<(), CoreError> {
    if uri.starts_with("spotify:track:") {
        return control::play_spotify_track(t, households, coordinator, uri, uri);
    }
    if let Some(kind) = fsonos_proto::didl::SpotifyContainer::of(uri) {
        return control::play_spotify_container(t, households, coordinator, kind, uri, uri);
    }
    control::play_uri(t, households, coordinator, uri, metadata)
}

/// [`play_source`] without the Play: the group is paused first if it
/// plays, then `uri` is selected, ready to resume, and no sound starts.
fn load_source<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    uri: &str,
    metadata: &str,
) -> Result<(), CoreError> {
    control::pause_confirmed(t, households, coordinator)?;
    if uri.starts_with("spotify:track:") {
        return control::load_spotify_track(t, households, coordinator, uri, uri);
    }
    if let Some(kind) = fsonos_proto::didl::SpotifyContainer::of(uri) {
        return control::load_spotify_container(t, households, coordinator, kind, uri, uri);
    }
    control::load_uri(t, households, coordinator, uri, metadata)
}

fn run<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    op: &SceneOp,
    options: &ApplyOptions<'_>,
    favorites: &mut HashMap<usize, Vec<Favorite>>,
) -> Result<bool, String> {
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
        SceneOp::Favorite {
            coordinator,
            name,
            start,
        } => {
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
            let done = if *start {
                favorites::play(t, households, coordinator, favorite)
            } else {
                favorites::load(t, households, coordinator, favorite)
            };
            done.map_err(|e| e.to_string())
        }
        SceneOp::Uri {
            coordinator,
            uri,
            metadata,
            start: true,
        } => play_source(t, households, coordinator, uri, metadata).map_err(s),
        SceneOp::Uri {
            coordinator,
            uri,
            metadata,
            start: false,
        } => load_source(t, households, coordinator, uri, metadata).map_err(s),
        SceneOp::Queue {
            coordinator,
            queue,
            start,
        } => {
            return queue::restore(t, households, coordinator, queue, *start);
        }
        SceneOp::Dj {
            coordinator,
            mood,
            direction,
        } => {
            options
                .hooks
                .start_dj_with_direction(coordinator, mood.as_deref(), direction.as_ref())
        }
        SceneOp::Play { coordinator } => control::resume(t, households, coordinator).map_err(s),
        // A stream that cannot pause stops (control::pause_at).
        SceneOp::Pause { coordinator } => control::pause(t, households, coordinator).map_err(s),
    }
    .map(|()| true)
}

/// A group the DJ is playing in as a scene is saved, and the mood its
/// steering names (`None`: the time-of-day program picks).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayingDj {
    pub coordinator: PlayerId,
    pub mood: Option<String>,
    pub direction: Option<DjDirection>,
}

/// The house as it is now, as a scene called `name`: every group, every
/// room's level, and what each group plays (the DJ where `djs` says it
/// plays, a favorite when the URI is one, a URI, or a guarded queue bookmark).
pub fn capture<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    name: &str,
    djs: &[PlayingDj],
    now: i64,
) -> Result<Scene, SceneError> {
    check_name(name)?;
    let mut scene = Scene {
        name: name.trim().to_string(),
        groups: Vec::new(),
        volumes: BTreeMap::new(),
        mutes: BTreeMap::new(),
        writer: None,
    };
    for (i, household) in households.iter().enumerate() {
        let room_name = |room: &Room| targets::captured_name(households, i, room);
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
            let before_boot = queue::boot(t, households, &group.coordinator)?;
            let snap = snapshot::capture(t, households, &group.coordinator, now)?;
            for level in &snap.levels {
                if let Some(r) = household.rooms.iter().find(|r| r.primary == level.player) {
                    let name = room_name(r)?;
                    scene.volumes.insert(name.clone(), level.volume);
                    scene.mutes.insert(name, level.mute);
                }
            }
            let dj = djs.iter().find(|d| d.coordinator == group.coordinator);
            let source = match (dj, &snap.source) {
                (Some(d), _) => SceneSource::Dj {
                    mood: d.mood.clone(),
                    direction: d.direction.clone(),
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
                (None, SnapshotSource::Queue { .. }) => {
                    queue::capture(t, households, &snap, before_boot)?
                        .map_or(SceneSource::Keep, |queue| SceneSource::Queue { queue })
                }
                (None, _) => SceneSource::Keep,
            };
            scene.groups.push(SceneGroup {
                coordinator: room_name(lead)?,
                members: household
                    .rooms
                    .iter()
                    .filter(|r| r.coordinator == group.coordinator && r.primary != lead.primary)
                    .map(room_name)
                    .collect::<Result<_, _>>()?,
                source,
                playing: Some(snap.transport_state == TransportState::Playing),
            });
        }
    }
    check(&scene, households)?;
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
            playing: Some(playing),
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
            play_mode: None,
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
            writer: None,
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
                    start: true,
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
    fn a_silent_group_the_scene_pauses_gets_its_new_source_loaded_not_played() {
        // C is to be paused on station b; it has paused (or stopped on)
        // station a.
        let h = house(&[&[&["A", "B"], &["C"]]]);
        let mut scene = evening();
        let b = "x-rincon-mp3radio://radio.example/b";
        scene.groups[1].source = SceneSource::Uri {
            uri: b.into(),
            metadata: String::new(),
        };
        let uri = |start| SceneOp::Uri {
            coordinator: pid("C"),
            uri: b.into(),
            metadata: String::new(),
            start,
        };
        let snaps = |now| {
            let mut snaps = evening_snaps();
            snaps[1].transport_state = now;
            snaps[1].source = radio();
            snaps
        };
        for now in [TransportState::Paused, TransportState::Stopped] {
            let plan = plan_apply(&scene, &h, &snaps(now)).unwrap();
            assert!(!uri(false).starts());
            assert_eq!(
                plan.ops,
                [uri(false)],
                "{now:?}: no Play, and no Pause after"
            );
        }
        // One that plays now is played and paused, as before: a source that
        // fails leaves its music playing.
        let plan = plan_apply(&scene, &h, &snaps(TransportState::Playing)).unwrap();
        assert_eq!(
            plan.ops,
            [
                uri(true),
                SceneOp::Pause {
                    coordinator: pid("C")
                }
            ]
        );
        // One whose state was not read may be playing: the same old path.
        let plan = plan_apply(&scene, &h, &evening_snaps()[..1]).unwrap();
        assert!(
            plan.ops.contains(&uri(true))
                && plan.ops.contains(&SceneOp::Pause {
                    coordinator: pid("C")
                }),
            "{:?}",
            plan.ops
        );
        // To play, it is played.
        scene.groups[1].playing = Some(true);
        let plan = plan_apply(&scene, &h, &evening_snaps()).unwrap();
        assert_eq!(plan.ops, [uri(true)]);
        assert!(uri(true).starts());
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
            writer: None,
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
        assert_eq!(minimal.groups[0].playing, None, "left as it is");
    }

    #[test]
    fn a_kept_group_that_says_nothing_of_playing_is_left_as_it_is() {
        // C plays now; the scene keeps what C has and says nothing of playing.
        let h = house(&[&[&["A", "B"], &["C"]]]);
        let mut snaps = evening_snaps();
        snaps[1].transport_state = TransportState::Playing;
        let mut scene = evening();
        scene.groups[1].playing = None;
        let plan = plan_apply(&scene, &h, &snaps).unwrap();
        assert_eq!(plan, ScenePlan::default(), "C is not paused");
        // Saying paused, it pauses.
        scene.groups[1].playing = Some(false);
        let plan = plan_apply(&scene, &h, &snaps).unwrap();
        assert_eq!(
            plan.ops,
            [SceneOp::Pause {
                coordinator: pid("C")
            }]
        );
    }
}
