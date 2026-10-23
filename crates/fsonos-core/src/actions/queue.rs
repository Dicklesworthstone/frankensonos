//! Undo confirmed native queue edits using durable contents and their exact
//! post-action result. Unsupported captures remain explicit undo barriers.

use crate::scenes::QueueBackup;
use crate::scenes::queue_backup::{self, Baseline};
use crate::snapshot::{self, Aspect, RestoreOp, RestoreReport, SnapshotSource, ZoneSnapshot};
use crate::{CoreError, HouseholdState};
use fsonos_proto::{ProtoError, Transport, control as soap};
use fsonos_types::{PlayerId, TransportState};
use serde::{Deserialize, Serialize};
use std::cell::Cell;
use std::time::Instant;

mod proof;
use proof::{Fenced, Proof};

fn unavailable(detail: impl std::fmt::Display) -> CoreError {
    CoreError::NotUndoable(format!("queue undo unavailable: {detail}"))
}

/// A bounded backup read before an edit. Bind it to the actual removal or
/// movement, then finish only with that operation's confirmed native result.
#[derive(Debug)]
pub struct PendingQueueUndo {
    snapshot: ZoneSnapshot,
    backup: QueueBackup,
    before: Proof,
    expected: Option<Expected>,
    observed: Instant,
}

#[derive(Debug)]
struct Expected {
    uris: Vec<String>,
    track: u32,
    removed_selection: bool,
}

impl PendingQueueUndo {
    /// The native revision whose full original contents were captured.
    #[must_use]
    pub fn revision(&self) -> u32 {
        self.before.revision
    }

    /// Original ordered renderer identities, including duplicate occurrences.
    #[must_use]
    pub fn uris(&self) -> &[String] {
        &self.before.uris
    }

    #[must_use]
    pub fn snapshot(&self) -> &ZoneSnapshot {
        &self.snapshot
    }

    /// A snapshot taken for undo must belong to the queue the edit will use.
    #[must_use]
    pub fn matches_before(&self, revision: u32, uris: &[String]) -> bool {
        revision != 0 && self.before.revision == revision && self.before.uris == uris
    }

    /// Bind the requested one-based removal and its exact occurrence change.
    pub fn removal(mut self, position: u32, count: u32) -> Result<Self, CoreError> {
        let (start, end) = selected(position, count, self.before.uris.len())?;
        let mut uris = self.before.uris.clone();
        uris.drain(start..end);
        let old = usize::try_from(self.before.playback.track - 1).map_err(unavailable)?;
        let removed_selection = (start..end).contains(&old);
        let slot = if old < start {
            old + 1
        } else if old >= end {
            old + 1 - (end - start)
        } else {
            (start + 1).min(uris.len())
        };
        self.expected = Some(Expected {
            uris,
            track: u32::try_from(slot).map_err(unavailable)?,
            removed_selection,
        });
        Ok(self)
    }

    /// Bind a native move. Both positions refer to the original queue.
    pub fn movement(
        mut self,
        position: u32,
        count: u32,
        insert_before: u32,
    ) -> Result<Self, CoreError> {
        let (start, end) = selected(position, count, self.before.uris.len())?;
        let at = insert_before
            .checked_sub(1)
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| *n <= self.before.uris.len())
            .ok_or_else(|| unavailable("invalid queue move destination"))?;
        if (start..=end).contains(&at) {
            return Err(unavailable("a no-op queue move needs no undo backup"));
        }
        let mut indices: Vec<_> = (0..self.before.uris.len()).collect();
        let block: Vec<_> = indices.drain(start..end).collect();
        let destination = if at < start { at } else { at - (end - start) };
        indices.splice(destination..destination, block);
        let old = usize::try_from(self.before.playback.track - 1).map_err(unavailable)?;
        let slot = indices
            .iter()
            .position(|index| *index == old)
            .ok_or_else(|| unavailable("the selected occurrence vanished from the move"))?;
        self.expected = Some(Expected {
            uris: indices
                .iter()
                .map(|index| self.before.uris[*index].clone())
                .collect(),
            track: u32::try_from(slot + 1).map_err(unavailable)?,
            removed_selection: false,
        });
        Ok(self)
    }
}

fn selected(position: u32, count: u32, length: usize) -> Result<(usize, usize), CoreError> {
    let start = position
        .checked_sub(1)
        .and_then(|n| usize::try_from(n).ok());
    let end = start.and_then(|n| n.checked_add(usize::try_from(count).ok()?));
    match (start, end) {
        (Some(start), Some(end)) if count > 0 && end <= length => Ok((start, end)),
        _ => Err(unavailable("the selected queue range is invalid")),
    }
}

/// Capture only a nonempty, selected own queue within the scene backup limits.
/// This is read-only; failure does not prohibit the requested edit, but its
/// caller must log an honest undo barrier instead of a partial backup.
pub fn capture_queue_undo<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    at: i64,
) -> Result<PendingQueueUndo, CoreError> {
    let started = Instant::now();
    let mut snapshot = snapshot::capture(t, households, coordinator, at)?;
    let SnapshotSource::Queue { track, .. } = snapshot.source else {
        return Err(unavailable(
            "only an actively selected own queue can be backed up",
        ));
    };
    if track == 0 {
        return Err(unavailable(
            "an empty or unselected queue has no playback bookmark",
        ));
    }
    let (initial, _) = Proof::read(t, households, coordinator, snapshot.play_mode.is_some(), at)?;
    let backup = queue_backup::capture(t, households, &snapshot, initial.binding.boot())?
        .ok_or_else(|| unavailable("no selected queue was captured"))?;
    let (before, _) = Proof::read(t, households, coordinator, snapshot.play_mode.is_some(), at)?;
    let mut levels = snapshot.levels.clone();
    levels.sort_by(|a, b| a.player.0.cmp(&b.player.0));
    if initial.binding != before.binding
        || initial.revision != before.revision
        || before.revision != backup.reference.update_id
        || levels != before.levels
        || before.uris
            != backup
                .entries
                .iter()
                .map(|entry| entry.uri.clone())
                .collect::<Vec<_>>()
        || before.playback.track != backup.reference.track
        || before.playback.mode != snapshot.play_mode
        || before.playback.state != snapshot.transport_state
        || !initial.playback.agrees(
            &before.playback,
            u32::try_from(started.elapsed().as_secs()).unwrap_or(u32::MAX),
        )
    {
        return Err(unavailable(
            "the queue, group or playback changed while saving its backup",
        ));
    }
    snapshot.source = SnapshotSource::Queue {
        track: backup.reference.track,
        position_secs: backup.reference.position_secs,
        update_id: backup.reference.update_id,
    };
    Ok(PendingQueueUndo {
        snapshot,
        backup,
        before,
        expected: None,
        observed: Instant::now(),
    })
}

/// Durable original contents and the exact state left by a confirmed edit.
/// Private fields keep renderer metadata out of ordinary preview DTOs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueUndo {
    version: u8,
    snapshot: ZoneSnapshot,
    backup: QueueBackup,
    after: Proof,
}

impl QueueUndo {
    #[must_use]
    pub fn coordinator(&self) -> &PlayerId {
        &self.snapshot.coordinator
    }

    #[must_use]
    pub fn snapshot(&self) -> &ZoneSnapshot {
        &self.snapshot
    }

    #[must_use]
    pub fn starts_playback(&self) -> bool {
        self.snapshot.transport_state == TransportState::Playing
    }

    #[must_use]
    pub fn has_play_mode(&self) -> bool {
        self.backup.reference.play_mode.is_some()
    }

    fn validate(&self) -> Result<(), CoreError> {
        self.backup.validate().map_err(unavailable)?;
        let saved = &self.backup.reference;
        if self.version != 1
            || self.snapshot.coordinator != saved.coordinator
            || self.snapshot.household != saved.household
            || self.after.binding.coordinator != saved.coordinator
            || self.after.binding.boot() != saved.boot_seq
            || self.after.revision == 0
            || self.after.revision == saved.update_id
            || self.after.uris.len() > self.backup.entries.len()
            || self.snapshot.play_mode != saved.play_mode
            || self.snapshot.source
                != (SnapshotSource::Queue {
                    track: saved.track,
                    position_secs: saved.position_secs,
                    update_id: saved.update_id,
                })
        {
            return Err(unavailable(
                "the saved queue action has inconsistent identity or contents",
            ));
        }
        Ok(())
    }
}

/// Complete capture only from the confirmed mutation's native revision and
/// ordered result. A fresh arbitrary read cannot make an uncertain edit undoable.
pub fn finish_queue_undo<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    pending: PendingQueueUndo,
    expected_revision: u32,
    expected_uris: &[String],
    at: i64,
) -> Result<QueueUndo, CoreError> {
    let expected = pending
        .expected
        .as_ref()
        .ok_or_else(|| unavailable("the queue edit was not bound before execution"))?;
    if expected_revision == 0
        || expected_revision == pending.before.revision
        || expected.uris != expected_uris
    {
        return Err(unavailable(
            "the confirmed revision or ordered result does not match the bound edit",
        ));
    }
    let (after, _) = Proof::read(
        t,
        households,
        &pending.snapshot.coordinator,
        pending.snapshot.play_mode.is_some(),
        at,
    )?;
    let before = &pending.before;
    let elapsed = u32::try_from(pending.observed.elapsed().as_secs()).unwrap_or(u32::MAX);
    let clock = before.playback.time_matches(&after.playback, elapsed)
        || (expected.removed_selection && after.playback.position == Some(0));
    let state = before.playback.state == after.playback.state
        || (expected.uris.is_empty() && after.playback.state == TransportState::Stopped);
    if after.revision != expected_revision
        || after.uris != expected_uris
        || before.binding != after.binding
        || before.levels != after.levels
        || before.playback.source != after.playback.source
        || before.playback.metadata != after.playback.metadata
        || after.playback.track != expected.track
        || before.playback.mode != after.playback.mode
        || (!expected.removed_selection && before.playback.duration != after.playback.duration)
        || !clock
        || !state
    {
        return Err(unavailable(
            "the confirmed edit's queue or playback ownership changed before logging",
        ));
    }
    let undo = QueueUndo {
        version: 1,
        snapshot: pending.snapshot,
        backup: pending.backup,
        after,
    };
    undo.validate()?;
    Ok(undo)
}

/// Attach queue recovery to the existing durable action JSON. Older action
/// shapes retain their meaning; unsupported queue actions keep their barrier.
#[must_use]
pub fn with_queue_undo(before: Option<String>, queues: &[QueueUndo]) -> Option<String> {
    if queues.is_empty() {
        return before;
    }
    let mut value = match before
        .as_deref()
        .map(serde_json::from_str::<serde_json::Value>)
        .transpose()
    {
        Ok(Some(value)) => value,
        Ok(None) => serde_json::json!({}),
        Err(_) => return before,
    };
    if value.is_array() {
        value = serde_json::json!({ "zones": value });
    }
    let Some(fields) = value.as_object_mut() else {
        return before;
    };
    if fields
        .get("not_undoable")
        .is_some_and(|value| !value.is_null())
    {
        return before;
    }
    if queues.iter().any(|queue| queue.validate().is_err()) {
        return Some(super::before_state_not_undoable(
            "the queue undo backup is invalid",
        ));
    }
    let zones = fields
        .entry("zones")
        .or_insert_with(|| serde_json::json!([]));
    let Some(zones) = zones.as_array_mut() else {
        return before;
    };
    for queue in queues {
        if !zones.iter().any(|zone| {
            zone.get("coordinator").and_then(serde_json::Value::as_str)
                == Some(queue.coordinator().0.as_str())
        }) {
            zones.push(serde_json::json!(queue.snapshot()));
        }
    }
    fields.insert("queue_undo".into(), serde_json::json!(queues));
    Some(value.to_string())
}

pub(super) fn refuse_if_changed<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    queues: &[QueueUndo],
    at: i64,
) -> Result<(), CoreError> {
    let started = Instant::now();
    let mut seen = Vec::new();
    for queue in queues {
        queue.validate()?;
        if seen.contains(&queue.coordinator()) {
            return Err(unavailable("the action repeats a queue coordinator"));
        }
        seen.push(queue.coordinator());
        queue.after.confirm(t, households, later(at, started))?;
    }
    Ok(())
}

/// The edit did not change room levels. Vet their current levels before an
/// audible restore and apply only policy reductions, carrying them into the
/// remaining ownership proof. No queue or playback write precedes this step.
fn cap_levels<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    proof: &mut Proof,
    at: i64,
    vet: &dyn Fn(&RestoreOp) -> Result<RestoreOp, String>,
) -> Result<bool, CoreError> {
    let started = Instant::now();
    let mut changed = false;
    let planned = proof
        .levels
        .iter()
        .map(|level| {
            let op = RestoreOp::SetVolume {
                player: level.player.clone(),
                level: level.volume,
            };
            match vet(&op).map_err(unavailable)? {
                RestoreOp::SetVolume {
                    player,
                    level: target,
                } if player == level.player && target <= level.volume => Ok((player, target)),
                _ => Err(unavailable(
                    "queue undo only permits policy reductions of current room levels",
                )),
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    for (player, target) in planned {
        let index = proof
            .levels
            .iter()
            .position(|level| level.player == player)
            .expect("planned level belongs to the proof");
        if proof.levels[index].volume == target {
            continue;
        }
        proof.confirm(t, households, later(at, started))?;
        let host = proof.binding.host(&player)?;
        let written = soap::set_volume(t, host, target);
        if matches!(
            &written,
            Err(ProtoError::SoapFault { .. } | ProtoError::NotWired(_))
        ) {
            return Err(written
                .expect_err("the result was a definite failure")
                .into());
        }
        proof.levels[index].volume = target;
        let confirmed = proof.confirm(t, households, later(at, started));
        match (written, confirmed) {
            (Ok(()), Ok(_)) | (Err(_), Ok(_)) => changed = true,
            (Err(error), Err(_)) => return Err(error.into()),
            (Ok(()), Err(error)) => return Err(error),
        }
    }
    Ok(changed)
}

pub(super) fn restore<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    saved: &QueueUndo,
    at: i64,
    vet: &dyn Fn(&RestoreOp) -> Result<RestoreOp, String>,
) -> Result<RestoreReport, CoreError> {
    let started = Instant::now();
    saved.validate()?;
    let permission = RestoreOp::RestoreQueue {
        player: saved.coordinator().clone(),
        start: saved.starts_playback(),
        mode: saved.has_play_mode(),
    };
    if vet(&permission).map_err(unavailable)? != permission {
        return Err(unavailable(
            "queue undo authorization changed the requested operation",
        ));
    }
    let mut proof = saved.after.clone();
    proof.confirm(t, households, later(at, started))?;
    let capped =
        saved.starts_playback() && cap_levels(t, households, &mut proof, later(at, started), vet)?;
    let owner = proof.confirm(t, households, later(at, started))?;
    let fenced = Fenced {
        t,
        households,
        proof: &proof,
        poisoned: Cell::new(false),
    };
    let baseline = Baseline {
        owner: &owner,
        revision: proof.revision,
        uris: &proof.uris,
    };
    queue_backup::restore_bound(
        &fenced,
        households,
        saved.coordinator(),
        &saved.backup,
        saved.starts_playback(),
        Some(&baseline),
    )
    .map_err(unavailable)?;
    if fenced.poisoned.get() {
        return Err(unavailable(
            "a reconstruction ownership fence failed; read the queue and playback before retrying",
        ));
    }
    let mut restored = vec![
        Aspect::Group,
        Aspect::Source,
        Aspect::Position,
        Aspect::Transport,
    ];
    if capped {
        restored.push(Aspect::Volume);
    }
    Ok(RestoreReport {
        restored,
        skipped: Vec::new(),
    })
}

pub(super) fn later(at: i64, started: Instant) -> i64 {
    at.saturating_add(i64::try_from(started.elapsed().as_secs()).unwrap_or(i64::MAX))
}
