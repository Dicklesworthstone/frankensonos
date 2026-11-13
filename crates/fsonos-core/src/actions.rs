//! The action log and undo.
//!
//! Every mutating request a surface carries out is logged with who asked
//! (the house-policy client), the surface, the policy's decision, what
//! happened, and snapshots of the zones it touched taken just before — denied
//! requests too, with nothing to undo. An action that steers the DJ also
//! carries the zones' DJ session rows as they were ([`SessionBefore`]).
//! An action that changes a room's tone carries the tone as it was
//! ([`ToneBefore`]), and one that adds to a group's queue what it added
//! ([`QueuedBefore`]). [`undo_last`] puts the newest undoable action's zones
//! back the way they were (via [`snapshot::restore`]), puts its DJ sessions
//! back in the store and its rooms' tones on the speakers, takes the tracks
//! it queued off again, and logs the undo
//! itself, which is never undoable (no redo).
//!
//! The row also keeps each touched room's state just after the action
//! ([`RoomAfter`]: its volume, mute, group and what the group plays). Undo
//! puts a zone back only while its rooms are still that way: one someone
//! changed since (the Sonos app, another caller), or whose current state cannot
//! be verified, is left as it is and named, unless the undo is forced
//! ([`undo_last_vetted`]).
//!
//! The surfaces own the flow (authorize, plan, bound by the policy, capture,
//! execute, [`record`]); this module owns the parts every surface shares, so
//! they all log and undo alike. Retention keeps the newest [`KEEP_ACTIONS`]
//! rows and nothing older than [`KEEP_SECS`].

use crate::control::{self, Tone, ToneChange};
use crate::policy::Client;
use crate::snapshot::{self, RestoreReport, ZoneSnapshot};
use crate::store::{Action, DjSession, LoggedAction, Store, StoreError};
use crate::{CoreError, HouseholdState};
use fsonos_proto::Transport;
use fsonos_types::PlayerId;
use serde::{Deserialize, Serialize};
use std::time::Instant;

mod house_config;
mod levels;
mod queue;
pub use house_config::{
    SceneBefore, SceneRow, ScheduleBefore, ScheduleRow, before_state_config, scene_row,
    schedule_row,
};
pub use levels::{LevelAfter, LevelScope, capture_level_after, with_level_after, with_level_scope};
pub use queue::{
    PendingQueueUndo, PendingSceneQueueUndo, QueueUndo, capture_queue_undo,
    capture_scene_queue_undo, finish_queue_replacement, finish_queue_undo,
    finish_scene_queue_replacement, with_queue_undo,
};

/// Retention: the newest this many actions are kept…
pub const KEEP_ACTIONS: usize = 10_000;
/// …and none older than 30 days.
pub const KEEP_SECS: i64 = 30 * 24 * 60 * 60;

fn store_err(e: &StoreError) -> CoreError {
    CoreError::Store(e.to_string())
}

/// Snapshot the zones led by `coordinators` (duplicates ignored) before an
/// action changes them. A zone that cannot be read is left out, with why: the
/// action still runs, it just cannot be fully undone.
pub fn capture_zones<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinators: &[PlayerId],
    at: i64,
) -> (Vec<ZoneSnapshot>, Vec<(PlayerId, String)>) {
    let mut snaps = Vec::new();
    let mut missed = Vec::new();
    let mut seen: Vec<&PlayerId> = Vec::new();
    for coordinator in coordinators {
        if seen.contains(&coordinator) {
            continue;
        }
        seen.push(coordinator);
        match snapshot::capture(t, households, coordinator, at) {
            Ok(snap) => snaps.push(snap),
            Err(e) => missed.push((coordinator.clone(), e.to_string())),
        }
    }
    (snaps, missed)
}

/// A zone's DJ session as it was before an action: undo saves `row` back, or
/// deletes the zone's session when there was none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionBefore {
    /// The coordinator the session is keyed by.
    pub coordinator: String,
    /// `None`: the zone had no session row (unsteered).
    pub row: Option<DjSession>,
}

/// A room's tone before an action changed it, addressed through the
/// room's primary player.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToneBefore {
    pub player: PlayerId,
    pub tone: Tone,
}

/// Tracks an action added to a group's queue (by coordinator): undo takes
/// them off again, while they are still where they were added.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedBefore {
    pub coordinator: PlayerId,
    /// The queue position (1-based) of the first.
    pub first: u32,
    /// What each added track renders, in queue order.
    pub uris: Vec<String>,
    /// Absent on legacy rows, which cannot prove ownership of a later queue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proof: Option<control::queue_removal::QueuedProof>,
}

/// Everything an action's undo puts back, as parsed from its log row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct BeforeState {
    #[serde(default)]
    pub zones: Vec<ZoneSnapshot>,
    #[serde(default)]
    pub dj_sessions: Vec<SessionBefore>,
    #[serde(default)]
    pub tones: Vec<ToneBefore>,
    #[serde(default)]
    pub queued: Vec<QueuedBefore>,
    /// Complete bounded contents and exact post-edit proof for native queue
    /// removal/reordering or replacement. Older action rows have no queue reconstruction.
    #[serde(default)]
    pub queue_undo: Vec<QueueUndo>,
    /// The touched rooms just after the action ([`with_after`]); none in
    /// rows logged before undo checked for changes.
    #[serde(default)]
    pub after: Vec<RoomAfter>,
    /// New level-only actions restore these exact players and fields. Missing
    /// in older rows, which retain their complete zone restore semantics.
    #[serde(default)]
    pub level_scope: Option<Vec<LevelScope>>,
    /// Field-specific proof after a scoped level action, independent of the
    /// group's source, transport, queue and other room settings.
    #[serde(default)]
    pub level_after: Vec<LevelAfter>,
    /// Set on a write undo cannot put back: what it was (a queue edit, a
    /// sleep timer, a fade-out). See [`before_state_not_undoable`].
    #[serde(default)]
    pub not_undoable: Option<String>,
    /// Scenes it saved over or deleted ([`before_state_config`]).
    #[serde(default)]
    pub scenes: Vec<SceneBefore>,
    /// Schedules it paused, resumed or removed.
    #[serde(default)]
    pub schedules: Vec<ScheduleBefore>,
}

/// A room as an action left it: what undo compares against before it
/// puts the room's zone back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomAfter {
    /// The room's primary player.
    pub player: PlayerId,
    pub volume: u8,
    pub mute: bool,
    /// The coordinator of the group it played in.
    pub coordinator: PlayerId,
    /// What that group played: its AVTransport URI (a queue, a stream, a
    /// track), or empty for nothing.
    pub source: String,
    /// For a room whose speaker was still ramping to `volume` when this was
    /// taken (a fade at its own pace): the level it ramped from. Any level
    /// between the two is the ramp, not a change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ramp_from: Option<u8>,
}

/// The state of `rooms` (primaries, duplicates ignored) now, read from the
/// players: a room that does not answer is left out. During undo, a missing
/// read for a still-addressable room cannot confirm its saved after-state.
pub fn rooms_after<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    rooms: &[PlayerId],
) -> Vec<RoomAfter> {
    let mut seen: Vec<&PlayerId> = Vec::new();
    // One read per group: its members play what its coordinator plays.
    let mut sources: Vec<(PlayerId, String)> = Vec::new();
    let mut after = Vec::new();
    for room in rooms {
        if seen.contains(&room) {
            continue;
        }
        seen.push(room);
        let Some(household) = households.iter().find(|h| h.player(room).is_some()) else {
            continue;
        };
        let coordinator = household.coordinator_of(room).unwrap_or(room).clone();
        let (Some(at), Some(lead)) = (household.player(room), household.player(&coordinator))
        else {
            continue;
        };
        let source = match sources.iter().find(|(c, _)| *c == coordinator) {
            Some((_, uri)) => uri.clone(),
            None => match fsonos_proto::control::get_media_info(t, lead.ip) {
                Ok(info) => {
                    sources.push((coordinator.clone(), info.uri.clone()));
                    info.uri
                }
                Err(_) => continue,
            },
        };
        let read = || -> Result<RoomAfter, fsonos_proto::ProtoError> {
            Ok(RoomAfter {
                player: room.clone(),
                volume: fsonos_proto::control::get_volume(t, at.ip)?,
                mute: fsonos_proto::control::get_mute(t, at.ip)?,
                coordinator: coordinator.clone(),
                source: source.clone(),
                ramp_from: None,
            })
        };
        if let Ok(room) = read() {
            after.push(room);
        }
    }
    after
}

/// `before` (an action's before-state) with the touched rooms' state after
/// it, `after`, for undo to compare against. Without a before-state there
/// is nothing to undo; without rooms, or with a before-state that does not
/// parse, it is returned as it was.
#[must_use]
pub fn with_after(before: Option<String>, after: &[RoomAfter]) -> Option<String> {
    let before = before?;
    if after.is_empty() {
        return Some(before);
    }
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&before) else {
        return Some(before);
    };
    if value.is_array() {
        value = serde_json::json!({ "zones": value });
    }
    let Some(fields) = value.as_object_mut() else {
        return Some(before);
    };
    fields.insert("after".into(), serde_json::json!(after));
    Some(value.to_string())
}

/// Whether a room's volume `now` differs from what the action left: for a
/// room still ramping there, a level short of the target on the way from
/// where it ramped from is the ramp, not a change.
fn volume_moved(was: &RoomAfter, now: u8) -> bool {
    let ramping = was
        .ramp_from
        .is_some_and(|from| (from.min(was.volume)..=from.max(was.volume)).contains(&now));
    now != was.volume && !ramping
}

/// A room's name, or its player id when the households no longer know it.
fn room_name(households: &[HouseholdState], id: &PlayerId) -> String {
    households
        .iter()
        .find_map(|h| h.player(id))
        .map_or_else(|| id.0.clone(), |p| p.room_name.clone())
}

/// What changed since the action in the rooms of `snap`'s zone, which undo
/// would put back: each room's volume, mute, group and source against
/// `after`, read again now, and any room grouped in since that is not one
/// of `touched` (the action's rooms, resolved in the same household), which
/// the restore would take out again. Unreadable rooms remain unverified, not
/// unchanged. A room absent from inventory has no restoration target and can
/// still be reported as skipped while surviving zones are restored. `None`
/// when no protected room changed or failed verification (or none was kept).
fn drift<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    after: &[RoomAfter],
    snap: &ZoneSnapshot,
    touched: &[PlayerId],
) -> Option<String> {
    let kept: Vec<&RoomAfter> = after
        .iter()
        .filter(|room| snap.members.contains(&room.player))
        .collect();
    let players: Vec<PlayerId> = kept.iter().map(|room| room.player.clone()).collect();
    let now = rooms_after(t, households, &players);
    let mut changes = Vec::new();
    for was in &kept {
        let room = room_name(households, &was.player);
        let Some(is) = now.iter().find(|room| room.player == was.player) else {
            if households.iter().any(|h| h.player(&was.player).is_some()) {
                changes.push(format!("{room}'s current state could not be verified"));
            }
            continue;
        };
        if volume_moved(was, is.volume) {
            changes.push(format!("{room}'s volume {} -> {}", was.volume, is.volume));
        }
        if is.mute != was.mute {
            changes.push(format!(
                "{room} {}",
                if is.mute { "muted" } else { "unmuted" }
            ));
        }
        if is.coordinator != was.coordinator {
            changes.push(format!("{room} regrouped"));
        } else if is.source != was.source {
            changes.push(format!("{room}'s group plays something else"));
        }
    }

    // A group member can be a satellite; resolve it to its logical room in
    // this household before asking whether that room was touched. Player ids
    // are global, while the same room name can exist in both households.
    let mut joined: Vec<String> = Vec::new();
    for household in households {
        for group in household
            .groups
            .iter()
            .filter(|g| kept.iter().any(|was| was.coordinator == g.coordinator))
        {
            for member in &group.members {
                let room = room_name(households, member);
                let was_touched = household.rooms.iter().any(|room| {
                    room.players.contains(member)
                        && room.players.iter().any(|player| touched.contains(player))
                });
                if !was_touched && !joined.contains(&room) {
                    joined.push(room);
                }
            }
        }
    }
    changes.extend(
        joined
            .into_iter()
            .map(|room| format!("{room} joined the group")),
    );
    (!changes.is_empty()).then(|| changes.join(", "))
}

/// Refuse to undo `target` when someone changed its zones since, or a saved
/// room's current state cannot be verified (see [`drift`]): nothing is put back
/// and nothing is logged, so the action stays undoable and a forced undo still
/// puts back this same action.
fn refuse_if_changed<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    target: &LoggedAction,
    before: &BeforeState,
) -> Result<(), CoreError> {
    if before.after.is_empty() {
        return Ok(());
    }
    let touched: Vec<PlayerId> = before
        .zones
        .iter()
        .flat_map(|snap| snap.members.iter())
        .chain(before.after.iter().map(|room| &room.player))
        .cloned()
        .collect();
    let changes: Vec<String> = before
        .zones
        .iter()
        .filter_map(|snap| drift(t, households, &before.after, snap, &touched))
        .collect();
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

impl BeforeState {
    /// Parse a logged before-state in either form: a bare array of zone
    /// snapshots (every row logged before sessions could be undone, and
    /// zones-only rows since) or `{"zones":[…],"dj_sessions":[…],"tones":[…],
    /// "queued":[…],"scenes":[…],"schedules":[…]}` (each part optional).
    ///
    /// # Errors
    /// The JSON is neither form.
    pub fn parse(json: &str) -> Result<Self, serde_json::Error> {
        let value: serde_json::Value = serde_json::from_str(json)?;
        if value.is_array() {
            return Ok(Self {
                zones: serde_json::from_value(value)?,
                ..Self::default()
            });
        }
        serde_json::from_value(value)
    }
}

/// The DJ sessions of the zones led by `coordinators` (duplicates ignored)
/// before an action changes them, for [`before_state_with`].
///
/// # Errors
/// The store cannot be read.
pub fn capture_sessions<S: Store + ?Sized>(
    store: &S,
    coordinators: &[PlayerId],
) -> Result<Vec<SessionBefore>, CoreError> {
    let mut sessions: Vec<SessionBefore> = Vec::new();
    for coordinator in coordinators {
        if sessions.iter().any(|s| s.coordinator == coordinator.0) {
            continue;
        }
        let row = store
            .dj_session(&coordinator.0)
            .map_err(|e| store_err(&e))?;
        sessions.push(SessionBefore {
            coordinator: coordinator.0.clone(),
            row,
        });
    }
    Ok(sessions)
}

/// Snapshots as an action's before-state; `None` when there are none, so the
/// action is not undoable.
#[must_use]
pub fn before_state(snaps: &[ZoneSnapshot]) -> Option<String> {
    before_state_with(snaps, &[])
}

/// Snapshots and DJ sessions as an action's before-state; `None` when both
/// are empty, so the action is not undoable. Zones alone keep the bare-array
/// form older rows use.
#[must_use]
pub fn before_state_with(snaps: &[ZoneSnapshot], sessions: &[SessionBefore]) -> Option<String> {
    if sessions.is_empty() {
        if snaps.is_empty() {
            return None;
        }
        return serde_json::to_string(snaps).ok();
    }
    Some(serde_json::json!({ "zones": snaps, "dj_sessions": sessions }).to_string())
}

/// Rooms' tones as an action's before-state; `None` when there are none.
#[must_use]
pub fn before_state_tones(tones: &[ToneBefore]) -> Option<String> {
    (!tones.is_empty()).then(|| serde_json::json!({ "tones": tones }).to_string())
}

/// Tracks added to queues as an action's before-state; `None` when there
/// are none.
#[must_use]
pub fn before_state_queued(queued: &[QueuedBefore]) -> Option<String> {
    if queued.is_empty() {
        return None;
    }
    let barrier = || before_state_not_undoable("queue addition without verified undo ownership");
    if queued.iter().any(|added| added.proof.is_none()) {
        return Some(barrier());
    }
    let json = serde_json::json!({ "queued": queued }).to_string();
    Some(if json.len() <= crate::scenes::queue_backup::MAX_QUEUE_BACKUP_BYTES {
        json
    } else {
        barrier()
    })
}

/// The before-state of a write that changed the speakers in a way undo
/// cannot put back, `what` it was ("queue edit"): undo stops at it rather
/// than reach past it to an older action ([`undo_target`]).
#[must_use]
pub fn before_state_not_undoable(what: &str) -> String {
    serde_json::json!({ "not_undoable": what }).to_string()
}

/// Whether a logged before-state is one undo can put back: there is one,
/// and it is not a [`before_state_not_undoable`] marker.
#[must_use]
pub fn undoable(before_state: Option<&str>) -> bool {
    before_state.is_some_and(|json| {
        !BeforeState::parse(json).is_ok_and(|before| before.not_undoable.is_some())
    })
}

/// Log `action`, then apply the retention policy. Returns the action's id.
pub fn record<S: Store + ?Sized>(store: &mut S, action: &Action) -> Result<i64, CoreError> {
    let id = store.record_action(action).map_err(|e| store_err(&e))?;
    store
        .prune_actions(KEEP_ACTIONS, action.at.saturating_sub(KEEP_SECS))
        .map_err(|e| store_err(&e))?;
    Ok(id)
}

/// How old an action may be and still be undone. Past this the house has
/// moved on: restoring what it was then would overwrite what people (the
/// Sonos app, other agents) did since.
pub const UNDO_MAX_AGE_SECS: i64 = 60 * 60;

/// The action [`undo_last_vetted`] would put back for `only` (or anyone) at
/// `at`, or `None` when there is nothing to undo; refused
/// ([`CoreError::NotUndoable`]) when it is older than [`UNDO_MAX_AGE_SECS`],
/// or when the newest write is one undo cannot put back
/// ([`before_state_not_undoable`]): undo never reaches past that to an older
/// action, which would leave the newer change in place and roll back what
/// came before it.
pub fn undo_target<S: Store + ?Sized>(
    store: &S,
    only: Option<&Client>,
    at: i64,
) -> Result<Option<LoggedAction>, CoreError> {
    let Some(target) = store
        .last_undoable_action(only.map(Client::key))
        .map_err(|e| store_err(&e))?
    else {
        return Ok(None);
    };
    let age = at.saturating_sub(target.action.at);
    if age > UNDO_MAX_AGE_SECS {
        return Err(CoreError::NotUndoable(format!(
            "#{} ({}) was {} minutes ago; undo goes back at most {} minutes, so the speakers \
             stay as they are now",
            target.id,
            target.action.intent,
            age / 60,
            UNDO_MAX_AGE_SECS / 60
        )));
    }
    let not_undoable = target
        .action
        .before_state
        .as_deref()
        .and_then(|json| BeforeState::parse(json).ok())
        .and_then(|before| before.not_undoable);
    if let Some(what) = not_undoable {
        return Err(CoreError::NotUndoable(format!(
            "#{} ({}) is a {what} undo can't put back, and undo never reaches past it to an \
             older action",
            target.id, target.action.intent
        )));
    }
    Ok(Some(target))
}

/// What [`undo_last`] did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UndoReport {
    /// The id of the action that was reversed, and what it was.
    pub undone: i64,
    pub intent: String,
    /// The id of the undo's own log entry.
    pub undo_id: i64,
    /// Per zone (by coordinator): what came back and what could not.
    pub zones: Vec<(PlayerId, RestoreReport)>,
    /// Zones that could not be restored at all, with why.
    pub failures: Vec<(PlayerId, String)>,
    /// DJ sessions put back (by coordinator); a zone that had none is
    /// unsteered again.
    pub sessions: Vec<PlayerId>,
    /// DJ sessions that could not be put back, with why.
    pub session_failures: Vec<(PlayerId, String)>,
    /// Rooms whose tone was put back (by primary player).
    pub tones: Vec<PlayerId>,
    /// Tones that could not be put back, with why.
    pub tone_failures: Vec<(PlayerId, String)>,
    /// Groups (by coordinator) whose queued tracks were taken off again.
    pub unqueued: Vec<PlayerId>,
    /// Queued tracks that could not be taken off, with why.
    pub unqueue_failures: Vec<(PlayerId, String)>,
    /// Scenes and schedules put back, named for people.
    pub configured: Vec<String>,
    /// Scenes and schedules that could not be put back, with why.
    pub config_failures: Vec<(String, String)>,
    /// One line for people.
    pub summary: String,
}

/// [`undo_last_vetted`] putting back every step as it was.
pub fn undo_last<T: Transport + ?Sized, S: Store + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    store: &mut S,
    only: Option<&Client>,
    by: &Client,
    surface: &str,
    at: i64,
) -> Result<Option<UndoReport>, CoreError> {
    undo_last_vetted(t, households, store, only, by, surface, at, false, &|op| {
        Ok(op.clone())
    })
}

/// Undo the newest undoable action — only `only`'s when given — on behalf of
/// `by` through `surface`: restore the zones it touched and the DJ sessions
/// it changed, log the undo, and report. `Ok(None)` when there is nothing to
/// undo.
///
/// The zones go back to exactly the state captured before that action;
/// undoing again steps further back. An action older than
/// [`UNDO_MAX_AGE_SECS`] is refused instead ([`CoreError::NotUndoable`], see
/// [`undo_target`]): the house has moved on since.
/// Each zone step passes through `vet` first ([`snapshot::restore_vetted`]).
/// When someone changed the action's rooms since (see [`RoomAfter`]), or a
/// still-addressable room cannot be verified, nothing is put back and the
/// undo is refused, naming the changed or unverified rooms
/// ([`CoreError::NotUndoable`]); the action stays undoable. `force` puts it
/// back anyway.
#[allow(clippy::too_many_arguments)] // undo_last's arguments, force and the vet
pub fn undo_last_vetted<T: Transport + ?Sized, S: Store + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    store: &mut S,
    only: Option<&Client>,
    by: &Client,
    surface: &str,
    at: i64,
    force: bool,
    vet: &dyn Fn(&snapshot::RestoreOp) -> Result<snapshot::RestoreOp, String>,
) -> Result<Option<UndoReport>, CoreError> {
    let Some(target) = undo_target(store, only, at)? else {
        return Ok(None);
    };
    restore_target_vetted(t, households, store, target, by, surface, at, force, vet).map(Some)
}

/// A reviewed undo refused before restoring, or failed during restoration.
#[derive(Debug, thiserror::Error)]
pub enum CheckedUndoError {
    /// The caller's latest eligible action is no longer the one reviewed.
    #[error("reviewed action #{expected} is no longer the newest undoable action (now {actual:?})")]
    TargetChanged { expected: i64, actual: Option<i64> },
    #[error(transparent)]
    Core(#[from] CoreError),
}

/// Undo exactly `expected`, but only while it is the newest eligible action.
/// Select once, check before any restoration I/O, then retain that selected
/// row. A later log insertion cannot substitute another action. The surface
/// must serialize this with every other undo; this does not lock the house
/// against ordinary speaker commands.
#[allow(clippy::too_many_arguments)] // undo_last_vetted plus the reviewed id
pub fn undo_last_vetted_checked<T: Transport + ?Sized, S: Store + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    store: &mut S,
    only: Option<&Client>,
    by: &Client,
    surface: &str,
    at: i64,
    expected: i64,
    force: bool,
    vet: &dyn Fn(&snapshot::RestoreOp) -> Result<snapshot::RestoreOp, String>,
) -> Result<UndoReport, CheckedUndoError> {
    let target = undo_target(store, only, at)?;
    let actual = target.as_ref().map(|row| row.id);
    let Some(target) = target.filter(|row| row.id == expected) else {
        return Err(CheckedUndoError::TargetChanged { expected, actual });
    };
    Ok(restore_target_vetted(
        t, households, store, target, by, surface, at, force, vet,
    )?)
}

/// What [`restore_zones`] put back per zone, and the zones it did not,
/// with why.
type ZoneOutcomes = (Vec<(PlayerId, RestoreReport)>, Vec<(PlayerId, String)>);

/// Put back each zone of `before` (see [`undo_last_vetted`]).
fn restore_zones<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    before: &BeforeState,
    at: i64,
    vet: &dyn Fn(&snapshot::RestoreOp) -> Result<snapshot::RestoreOp, String>,
) -> ZoneOutcomes {
    if before.level_scope.is_some() {
        return levels::restore(t, households, before, vet);
    }
    let started = Instant::now();
    let mut zones = Vec::new();
    let mut failures = Vec::new();
    for saved in &before.queue_undo {
        match queue::restore(t, households, saved, queue::later(at, started), vet) {
            Ok(report) => zones.push((saved.coordinator().clone(), report)),
            Err(error) => failures.push((saved.coordinator().clone(), error.to_string())),
        }
    }
    for snap in &before.zones {
        // A queue reconstruction owns its complete playback continuation.
        // Never follow its failure with an obsolete reference-only restore.
        if before
            .queue_undo
            .iter()
            .any(|saved| saved.coordinator() == &snap.coordinator)
        {
            continue;
        }
        match snapshot::restore_vetted(t, households, snap, &snapshot::Aspect::ALL, vet) {
            Ok(report) => zones.push((snap.coordinator.clone(), report)),
            Err(e) => failures.push((snap.coordinator.clone(), e.to_string())),
        }
    }
    (zones, failures)
}

#[allow(clippy::too_many_arguments)] // the selected row replaces undo_last's filter
fn restore_target_vetted<T: Transport + ?Sized, S: Store + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    store: &mut S,
    target: LoggedAction,
    by: &Client,
    surface: &str,
    at: i64,
    force: bool,
    vet: &dyn Fn(&snapshot::RestoreOp) -> Result<snapshot::RestoreOp, String>,
) -> Result<UndoReport, CoreError> {
    let started = Instant::now();
    let mut before = BeforeState::parse(target.action.before_state.as_deref().unwrap_or("[]"))
        .map_err(|e| {
            CoreError::Store(format!(
                "action {} has an unreadable before-state: {e}",
                target.id
            ))
        })?;
    // Even --force cannot overwrite a queue that no longer matches the exact
    // confirmed edit. Check every saved queue before any independent restore.
    rebase_queued(store, target.id, &mut before.queued)?;
    refuse_queued_if_changed(t, households, &before.queued)?;
    queue::refuse_if_changed(t, households, &before.queue_undo, at)?;
    // A scope is never permission to invent a missing prior level, including
    // on a forced undo. Validate the whole restore before any writes.
    levels::validate(households, &before)?;
    let level_state = before
        .level_scope
        .as_ref()
        .map(|_| levels::observe(t, households, &target, &before, force))
        .transpose()?;
    if !force {
        if level_state.is_none() {
            refuse_if_changed(t, households, &target, &before)?;
        }
        house_config::refuse_if_changed(store, &target, &before)?;
    }
    let coordinators: Vec<PlayerId> = before.zones.iter().map(|s| s.coordinator.clone()).collect();
    // A level-only undo must not depend on playback reads or stop a later
    // queue occurrence. Its own audit row keeps just the levels it observed;
    // undo rows cannot be undone, so they need no full restoration snapshot.
    let now = if level_state.is_some() {
        Vec::new()
    } else {
        capture_zones(t, households, &coordinators, at).0
    };
    let steered: Vec<PlayerId> = before
        .dj_sessions
        .iter()
        .map(|s| PlayerId(s.coordinator.clone()))
        .collect();
    let now_sessions = capture_sessions(store, &steered)?;

    let (zones, failures) = restore_zones(t, households, &before, queue::later(at, started), vet);
    let (sessions, session_failures) = put_back_sessions(store, &before.dj_sessions);
    let (tones, tone_failures) = put_back_tones(t, households, &before.tones);
    let ((unqueued, unqueue_failures), restored_queues) =
        take_off_queued(t, households, &before.queued);
    let config = house_config::put_back_config(store, &before, at);
    let mut others = outcome_lines(
        "steering for",
        "put back",
        &named(households, &sessions),
        &session_failures,
    );
    others.extend(outcome_lines(
        "the tone of",
        "put back",
        &named(households, &tones),
        &tone_failures,
    ));
    others.extend(outcome_lines(
        "the tracks queued in",
        "taken off",
        &named(households, &unqueued),
        &unqueue_failures,
    ));
    others.extend(config.lines());
    let summary = summarize(target.id, &target.action.intent, &zones, &failures, &others);
    let undo_id = record(
        store,
        &Action {
            at,
            client: by.key().to_string(),
            surface: surface.to_string(),
            intent: format!("undo #{}: {}", target.id, target.action.intent),
            decision: "allow".into(),
            result: summary.clone(),
            before_state: with_queued_restorations(
                level_state.map_or_else(
                    || before_state_with(&now, &now_sessions),
                    |levels| {
                        Some(
                            serde_json::json!({ "levels": levels, "dj_sessions": now_sessions })
                                .to_string(),
                        )
                    },
                ),
                &restored_queues,
            ),
            undo_of: Some(target.id),
        },
    )?;
    Ok(UndoReport {
        undone: target.id,
        intent: target.action.intent,
        undo_id,
        zones,
        failures,
        sessions,
        session_failures,
        tones,
        tone_failures,
        unqueued,
        unqueue_failures,
        configured: config.done,
        config_failures: config.failed,
        summary,
    })
}

/// Each coordinator's room name, or its id when the households no longer
/// know it.
fn named(households: &[HouseholdState], coordinators: &[PlayerId]) -> Vec<String> {
    coordinators
        .iter()
        .map(|id| {
            households
                .iter()
                .find_map(|h| h.player(id))
                .map_or_else(|| id.0.clone(), |p| p.room_name.clone())
        })
        .collect()
}

/// What an undo put back, and what it could not (with why).
type Outcomes = (Vec<PlayerId>, Vec<(PlayerId, String)>);

/// Put back each coordinator's retained DJ session.
fn put_back_sessions<S: Store + ?Sized>(store: &mut S, before: &[SessionBefore]) -> Outcomes {
    let mut sessions = Vec::new();
    let mut session_failures = Vec::new();
    for session in before {
        let put_back = match &session.row {
            Some(row) => store.save_dj_session(row),
            None => store.delete_dj_session(&session.coordinator),
        };
        let coordinator = PlayerId(session.coordinator.clone());
        match put_back {
            Ok(()) => sessions.push(coordinator),
            Err(e) => session_failures.push((coordinator, e.to_string())),
        }
    }
    (sessions, session_failures)
}

/// Each room's tone in `tones` put back on its player.
fn put_back_tones<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    tones: &[ToneBefore],
) -> Outcomes {
    let (mut done, mut failed) = (Vec::new(), Vec::new());
    for was in tones {
        match control::set_tone(t, households, &was.player, ToneChange::all(was.tone)) {
            Ok(()) => done.push(was.player.clone()),
            Err(e) => failed.push((was.player.clone(), e.to_string())),
        }
    }
    (done, failed)
}

/// Follow only logged, fully verified native undos, in chronological order.
/// Equality of today's track URIs alone can never advance a queue revision.
fn rebase_queued<S: Store + ?Sized>(
    store: &S,
    target: i64,
    queued: &mut [QueuedBefore],
) -> Result<(), CoreError> {
    if queued.is_empty() {
        return Ok(());
    }
    if queued.iter().any(|added| added.proof.is_none()) {
        return Err(CoreError::NotUndoable(
            "this older queue addition has no retained ownership proof; force cannot authorize deleting later occurrences".into(),
        ));
    }
    #[derive(Deserialize)]
    struct Transitions {
        #[serde(default)]
        queued_restored: Vec<control::queue_removal::QueuedRestoration>,
    }
    // A missing old transition is a conservative refusal, never permission to
    // infer that an equal-looking queue still contains the original occurrences.
    let mut rows = store
        .recent_actions(&crate::store::ActionFilter {
            limit: 256,
            ..Default::default()
        })
        .map_err(|error| store_err(&error))?;
    rows.sort_by_key(|row| row.id);
    for row in rows {
        if row.id <= target || row.action.undo_of.is_none() {
            continue;
        }
        let Some(json) = row.action.before_state.as_deref() else {
            continue;
        };
        if json.len() > crate::scenes::queue_backup::MAX_QUEUE_BACKUP_BYTES {
            continue;
        }
        let Ok(transitions) = serde_json::from_str::<Transitions>(json) else {
            continue;
        };
        if transitions.queued_restored.len() > 256 {
            continue;
        }
        for transition in &transitions.queued_restored {
            for added in &mut *queued {
                if let Some(proof) = &mut added.proof {
                    proof.rebase(transition);
                }
            }
        }
    }
    Ok(())
}

/// Validate every queued addition before any independent restore, even forced.
fn refuse_queued_if_changed<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    queued: &[QueuedBefore],
) -> Result<(), CoreError> {
    for added in queued {
        let proof = added.proof.as_ref().ok_or_else(|| {
            CoreError::NotUndoable(
                "queued-track undo has no retained ownership proof; force cannot bypass it".into(),
            )
        })?;
        let host = control::locate(households, &added.coordinator)?.ip;
        proof.confirm(t, host, &added.coordinator, added.first, &added.uris)?;
    }
    Ok(())
}

/// Persist native undo receipts with the audit row, so stacked undo survives a
/// daemon restart. These are internal evidence, never supplied by a client.
fn with_queued_restorations(
    before: Option<String>,
    restored: &[control::queue_removal::QueuedRestoration],
) -> Option<String> {
    if restored.is_empty() {
        return before;
    }
    let mut value = before
        .as_deref()
        .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if value.is_array() {
        value = serde_json::json!({ "zones": value });
    }
    let Some(object) = value.as_object_mut() else {
        return before;
    };
    object.insert("queued_restored".into(), serde_json::json!(restored));
    let json = value.to_string();
    if json.len() > crate::scenes::queue_backup::MAX_QUEUE_BACKUP_BYTES {
        return before;
    }
    Some(json)
}

/// The tracks queued by an action taken off their groups' queues again.
fn take_off_queued<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    queued: &[QueuedBefore],
) -> (Outcomes, Vec<control::queue_removal::QueuedRestoration>) {
    let (mut done, mut failed, mut restored) = (Vec::new(), Vec::new(), Vec::new());
    for added in queued {
        let coordinator = added.coordinator.clone();
        let outcome = (|| {
            let proof = added.proof.as_ref().ok_or_else(|| {
                CoreError::NotUndoable("queued-track undo has no retained ownership proof".into())
            })?;
            let host = control::locate(households, &coordinator)?.ip;
            proof.undo(t, host, &coordinator, added.first, &added.uris)
        })();
        match outcome {
            Ok(transition) => {
                done.push(coordinator);
                restored.extend(transition);
            }
            Err(error) => failed.push((coordinator, error.to_string())),
        }
    }
    // A partial multi-group undo cannot grant a revision rebase.
    if !failed.is_empty() {
        restored.clear();
    }
    ((done, failed), restored)
}

/// `{what} {names} {done}`, and `{what} {id} not {done}: {why}` per
/// failure: what an undo put back besides the zones.
fn outcome_lines(
    what: &str,
    done: &str,
    names: &[String],
    failures: &[(PlayerId, String)],
) -> Vec<String> {
    let mut lines = Vec::new();
    if !names.is_empty() {
        lines.push(format!("{what} {} {done}", names.join(", ")));
    }
    for (id, why) in failures {
        lines.push(format!("{what} {id} not {done}: {why}", id = id.0));
    }
    lines
}

fn summarize(
    id: i64,
    intent: &str,
    zones: &[(PlayerId, RestoreReport)],
    failures: &[(PlayerId, String)],
    others: &[String],
) -> String {
    let mut parts = Vec::new();
    if !zones.is_empty() || !failures.is_empty() || others.is_empty() {
        parts.push(format!(
            "restored {} of {} zone(s)",
            zones.len(),
            zones.len() + failures.len()
        ));
    }
    for (_, report) in zones {
        for (aspect, why) in &report.skipped {
            parts.push(format!("{aspect:?} not restored: {why}").to_lowercase());
        }
    }
    for (coordinator, why) in failures {
        parts.push(format!("{} failed: {why}", coordinator.0));
    }
    parts.extend(others.iter().cloned());
    format!("undid #{id} ({intent}): {}", parts.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{Aspect, SnapshotSource};
    use crate::store::{ActionFilter, MemStore};
    use fsonos_types::TransportState;

    fn snap(coordinator: &str) -> ZoneSnapshot {
        ZoneSnapshot {
            household: None,
            coordinator: PlayerId(coordinator.into()),
            members: vec![PlayerId(coordinator.into())],
            levels: Vec::new(),
            group_volume: Some(20),
            transport_state: TransportState::Playing,
            play_mode: None,
            source: SnapshotSource::Uri {
                uri: "x-rincon-mp3radio://example.invalid/stream".into(),
                metadata: String::new(),
                position_secs: None,
            },
            captured_at: 5,
        }
    }

    fn action(at: i64, before: Option<String>) -> Action {
        Action {
            at,
            client: "tag:agent".into(),
            surface: "mcp".into(),
            intent: "volume Kitchen 90".into(),
            decision: "clamp: Kitchen is capped at 70".into(),
            result: "Kitchen volume 70".into(),
            before_state: before,
            undo_of: None,
        }
    }

    #[test]
    fn before_state_round_trips_and_is_none_when_empty() {
        assert_eq!(before_state(&[]), None);
        let json = before_state(&[snap("RINCON_A"), snap("RINCON_B")]).unwrap();
        let back: Vec<ZoneSnapshot> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, [snap("RINCON_A"), snap("RINCON_B")]);
    }

    #[test]
    fn record_prunes_by_age_and_count() {
        let mut store = MemStore::default();
        let old = record(&mut store, &action(0, None)).unwrap();
        let fresh = record(&mut store, &action(KEEP_SECS + 10, None)).unwrap();
        let log = store.recent_actions(&ActionFilter::default()).unwrap();
        assert_eq!(log.iter().map(|a| a.id).collect::<Vec<_>>(), [fresh]);
        assert!(fresh > old);
    }

    #[test]
    fn nothing_to_undo_is_none() {
        struct NoLan;
        impl Transport for NoLan {
            fn soap_post(
                &self,
                _: std::net::IpAddr,
                _: &str,
                _: &str,
                _: &str,
            ) -> Result<String, fsonos_proto::ProtoError> {
                Err(fsonos_proto::ProtoError::NotWired("no LAN"))
            }
        }
        let mut store = MemStore::default();
        // A denial has no before-state: nothing to undo.
        let denied = Action {
            decision: "deny: the agent may not set volumes".into(),
            ..action(1, None)
        };
        record(&mut store, &denied).unwrap();
        let done = undo_last(&NoLan, &[], &mut store, None, &Client::Cli, "cli", 2).unwrap();
        assert_eq!(done, None);
    }

    /// Fails every call: the refusals below must come before any speaker is
    /// touched.
    struct Unreached;
    impl Transport for Unreached {
        fn soap_post(
            &self,
            _: std::net::IpAddr,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<String, fsonos_proto::ProtoError> {
            panic!("undo touched a speaker before refusing")
        }
    }

    #[test]
    fn the_rooms_after_join_the_before_state_and_parse_back() {
        let after = [RoomAfter {
            player: PlayerId("RINCON_A".into()),
            volume: 30,
            mute: false,
            coordinator: PlayerId("RINCON_A".into()),
            source: "x-rincon-queue:RINCON_A#0".into(),
            ramp_from: None,
        }];
        assert_eq!(with_after(None, &after), None, "nothing to undo stays so");
        let zones = before_state(&[snap("RINCON_A")]);
        assert_eq!(with_after(zones.clone(), &[]), zones, "no rooms: as it was");
        let joined = with_after(zones, &after).unwrap();
        let parsed = BeforeState::parse(&joined).unwrap();
        assert_eq!(parsed.zones, [snap("RINCON_A")]);
        assert_eq!(parsed.after, after);
        // Rows logged before the after-state parse with none.
        assert!(
            BeforeState::parse(&before_state(&[snap("RINCON_A")]).unwrap())
                .unwrap()
                .after
                .is_empty()
        );
    }

    #[test]
    fn a_room_still_ramping_to_its_level_has_not_moved() {
        let was = |ramp_from| RoomAfter {
            player: PlayerId("RINCON_A".into()),
            volume: 10,
            mute: false,
            coordinator: PlayerId("RINCON_A".into()),
            source: String::new(),
            ramp_from,
        };
        assert!(!volume_moved(&was(None), 10));
        assert!(
            volume_moved(&was(None), 25),
            "no ramp: any other level moved"
        );
        // Fading 50 -> 10 at the speaker's own pace: 25 is on the way.
        assert!(!volume_moved(&was(Some(50)), 25));
        assert!(!volume_moved(&was(Some(50)), 50), "not started yet");
        assert!(
            volume_moved(&was(Some(50)), 60),
            "above where it ramped from"
        );
        assert!(volume_moved(&was(Some(50)), 5), "past its target");
        // And up: 10 -> 40.
        let up = RoomAfter {
            volume: 40,
            ..was(Some(10))
        };
        assert!(!volume_moved(&up, 30));
        assert!(volume_moved(&up, 45));
    }

    #[test]
    fn an_action_older_than_the_limit_is_not_undone() {
        let mut store = MemStore::default();
        record(&mut store, &action(100, before_state(&[snap("RINCON_A")]))).unwrap();
        let late = 100 + UNDO_MAX_AGE_SECS + 1;
        let err =
            undo_last(&Unreached, &[], &mut store, None, &Client::Cli, "cli", late).unwrap_err();
        assert!(
            matches!(&err, CoreError::NotUndoable(why) if why.contains("60 minutes")),
            "{err}"
        );
    }

    #[test]
    fn undo_stops_at_a_write_it_cannot_put_back() {
        let mut store = MemStore::default();
        record(&mut store, &action(100, before_state(&[snap("RINCON_A")]))).unwrap();
        let edit = record(
            &mut store,
            &action(110, Some(before_state_not_undoable("queue edit"))),
        )
        .unwrap();
        // A no-op since (nothing changed, nothing to put back) is no write.
        record(&mut store, &action(115, None)).unwrap();
        let err =
            undo_last(&Unreached, &[], &mut store, None, &Client::Cli, "cli", 120).unwrap_err();
        assert!(
            matches!(&err, CoreError::NotUndoable(why)
                if why.starts_with(&format!("#{edit} ")) && why.contains("queue edit")),
            "{err}"
        );
        assert!(!undoable(Some(&before_state_not_undoable("queue edit"))));
        assert!(undoable(before_state(&[snap("RINCON_A")]).as_deref()));
        assert!(!undoable(None));
    }

    #[test]
    fn summary_names_what_did_not_come_back() {
        let report = RestoreReport {
            restored: vec![Aspect::Volume],
            skipped: vec![(Aspect::Position, "the queue changed".into())],
        };
        let text = summarize(
            7,
            "volume Kitchen 90",
            &[(PlayerId("RINCON_A".into()), report)],
            &[(PlayerId("RINCON_B".into()), "unknown player".into())],
            &[],
        );
        assert_eq!(
            text,
            "undid #7 (volume Kitchen 90): restored 1 of 2 zone(s); position not restored: \
             the queue changed; RINCON_B failed: unknown player"
        );
    }

    #[test]
    fn summary_of_a_pure_steer_names_the_steering() {
        let text = summarize(
            8,
            "dj steer Kitchen calm",
            &[],
            &[],
            &outcome_lines(
                "steering for",
                "put back",
                &["Kitchen".into()],
                &[(PlayerId("RINCON_B".into()), "disk full".into())],
            ),
        );
        assert_eq!(
            text,
            "undid #8 (dj steer Kitchen calm): steering for Kitchen put back; \
             steering for RINCON_B not put back: disk full"
        );
    }

    fn session(coordinator: &str, row: Option<&str>) -> SessionBefore {
        SessionBefore {
            coordinator: coordinator.into(),
            row: row.map(|mood| DjSession {
                coordinator: coordinator.into(),
                mood: Some(mood.into()),
                constraints: Some(r#"{"energy":[0.2,0.5]}"#.into()),
                expires: 900,
            }),
        }
    }

    #[test]
    fn sessions_make_the_object_form_and_zones_alone_stay_a_bare_array() {
        assert_eq!(before_state_with(&[], &[]), None);
        let zones_only = before_state_with(&[snap("RINCON_A")], &[]).unwrap();
        assert!(zones_only.starts_with('['), "{zones_only}");
        assert_eq!(
            BeforeState::parse(&zones_only).unwrap(),
            BeforeState {
                zones: vec![snap("RINCON_A")],
                ..BeforeState::default()
            }
        );

        let sessions = [session("RINCON_A", Some("calm")), session("RINCON_B", None)];
        let json = before_state_with(&[snap("RINCON_A")], &sessions).unwrap();
        assert!(json.starts_with('{'), "{json}");
        assert_eq!(
            BeforeState::parse(&json).unwrap(),
            BeforeState {
                zones: vec![snap("RINCON_A")],
                dj_sessions: sessions.to_vec(),
                ..BeforeState::default()
            }
        );
        let steer_only = before_state_with(&[], &sessions[1..]).unwrap();
        assert_eq!(
            BeforeState::parse(&steer_only).unwrap().dj_sessions,
            sessions[1..]
        );
        assert!(BeforeState::parse(r#""zones""#).is_err());
    }

    #[test]
    fn legacy_queued_tracks_parse_but_new_additions_without_proof_are_barriers() {
        assert_eq!(before_state_queued(&[]), None);
        let added = QueuedBefore {
            coordinator: PlayerId("RINCON_A".into()),
            first: 3,
            uris: vec!["x-sonos-spotify:spotify%3atrack%3aX".into()],
            proof: None,
        };
        let barrier = before_state_queued(std::slice::from_ref(&added)).unwrap();
        assert!(!undoable(Some(&barrier)));
        let json = serde_json::json!({ "queued": [&added] }).to_string();
        assert_eq!(
            BeforeState::parse(&json).unwrap(),
            BeforeState {
                queued: vec![added],
                ..BeforeState::default()
            }
        );
        assert_eq!(
            outcome_lines(
                "the tracks queued in",
                "taken off",
                &[],
                &[(PlayerId("RINCON_A".into()), "the queue changed".into())],
            ),
            ["the tracks queued in RINCON_A not taken off: the queue changed"]
        );
    }

    #[test]
    fn a_tone_alone_is_undoable_and_its_undo_names_the_room() {
        assert_eq!(before_state_tones(&[]), None);
        let was = ToneBefore {
            player: PlayerId("RINCON_A".into()),
            tone: Tone {
                bass: -2,
                treble: 3,
                loudness: true,
            },
        };
        let json = before_state_tones(std::slice::from_ref(&was)).unwrap();
        assert_eq!(
            BeforeState::parse(&json).unwrap(),
            BeforeState {
                tones: vec![was],
                ..BeforeState::default()
            }
        );
        let text = summarize(
            9,
            "set_eq: Kitchen bass +4",
            &[],
            &[],
            &outcome_lines(
                "the tone of",
                "put back",
                &["Kitchen".into()],
                &[(PlayerId("RINCON_B".into()), "unreachable".into())],
            ),
        );
        assert_eq!(
            text,
            "undid #9 (set_eq: Kitchen bass +4): the tone of Kitchen put back; \
             the tone of RINCON_B not put back: unreachable"
        );
    }
}

#[cfg(test)]
mod checked_tests;
