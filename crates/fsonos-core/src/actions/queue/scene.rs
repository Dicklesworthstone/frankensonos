//! Scene replacements retain their own level changes without relaxing the
//! invariants of direct playback or native queue edits. The physical group
//! must stay unchanged and every scene step must complete before publication.

use super::proof::{Fenced, Playback, Proof};
use super::{PendingQueueUndo, QueueUndo, cap_levels, capture_queue_undo, later, unavailable};
use crate::actions::LevelScope;
use crate::control::QueueReplacement;
use crate::scenes::{SceneOp, ScenePlan, SceneReport, queue, queue_backup};
use crate::snapshot::{Aspect, MemberLevel, RestoreOp, RestoreReport};
use crate::{CoreError, HouseholdState};
use fsonos_proto::{ProtoError, Transport, control as soap};
use fsonos_types::{PlayMode, PlayerId, TransportState};
use serde::{Deserialize, Serialize};
use std::cell::Cell;
use std::time::Instant;

/// A selected original queue and the exact level effects of its scene plan.
/// Capture after policy cap reductions and before any scene execution. The
/// capped original levels stay capped when the action is later undone.
#[derive(Debug)]
pub struct PendingSceneQueueUndo {
    pending: PendingQueueUndo,
    plan: ScenePlan,
    source: SceneOp,
    levels: Vec<MemberLevel>,
    restore: SceneLevels,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct SceneLevels {
    scope: Vec<LevelScope>,
    playing: bool,
}

/// Capture a bounded original queue for one replacement on an unchanged
/// group. Regrouping, DJ changes and queue-backup recalls remain barriers for
/// the entire scene; unsupported scenes still execute through their usual path.
pub fn capture_scene_queue_undo<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    plan: &ScenePlan,
    at: i64,
) -> Result<PendingSceneQueueUndo, CoreError> {
    if plan.ops.iter().any(|op| {
        matches!(
            op,
            SceneOp::Leave { .. }
                | SceneOp::Join { .. }
                | SceneOp::QueueBackup { .. }
                | SceneOp::Dj { .. }
        )
    }) {
        return Err(unavailable(
            "scene queue undo requires unchanged groups without DJ or queue-backup changes",
        ));
    }
    let sources: Vec<_> = plan
        .ops
        .iter()
        .filter_map(|op| match op {
            SceneOp::Favorite {
                coordinator: id,
                start,
                ..
            }
            | SceneOp::SonosPlaylist {
                coordinator: id,
                start,
                ..
            }
            | SceneOp::Uri {
                coordinator: id,
                start,
                ..
            } if id == coordinator => Some((op.clone(), *start)),
            _ => None,
        })
        .collect();
    let [(source, playing)] = sources.as_slice() else {
        return Err(unavailable("the scene must have one source for this queue"));
    };
    if plan.ops.iter().any(|op| {
        matches!(op, SceneOp::Play { coordinator: id } | SceneOp::Pause { coordinator: id }
            | SceneOp::Queue { coordinator: id, .. } if id == coordinator)
    }) {
        return Err(unavailable(
            "the scene has another playback operation for this queue",
        ));
    }
    let pending = capture_queue_undo(t, households, coordinator, at)?;
    let mut levels = pending.before.levels.clone();
    let mut scope: Vec<LevelScope> = Vec::new();
    for op in &plan.ops {
        let (player, volume) = match op {
            SceneOp::Volume { player, .. } => (player, true),
            SceneOp::Mute { player, .. } => (player, false),
            _ => continue,
        };
        let Some(level) = levels.iter_mut().find(|level| level.player == *player) else {
            continue;
        };
        let index = scope
            .iter()
            .position(|scope| scope.player == *player)
            .unwrap_or_else(|| {
                scope.push(LevelScope {
                    player: player.clone(),
                    volume: false,
                    mute: false,
                });
                scope.len() - 1
            });
        let field = if volume {
            &mut scope[index].volume
        } else {
            &mut scope[index].mute
        };
        if *field {
            return Err(unavailable("the scene repeats a room's level field"));
        }
        *field = true;
        match op {
            SceneOp::Volume { level: target, .. } if *target <= 100 => level.volume = *target,
            SceneOp::Mute { mute, .. } => level.mute = *mute,
            _ => return Err(unavailable("the scene has an invalid room level")),
        }
    }
    Ok(PendingSceneQueueUndo {
        pending,
        plan: plan.clone(),
        source: source.clone(),
        levels,
        restore: SceneLevels {
            scope,
            playing: *playing,
        },
    })
}

fn selected(after: &Proof, playing: bool) -> bool {
    super::replacement::valid_queue(after)
        && if playing {
            after.playback.state == TransportState::Playing
        } else {
            matches!(
                after.playback.state,
                TransportState::Paused | TransportState::Stopped
            )
        }
}

pub(super) fn valid_result(saved: &QueueUndo) -> bool {
    let Some(scene) = &saved.scene else {
        return false;
    };
    if !selected(&saved.after, scene.playing) {
        return false;
    }
    let mut seen = Vec::new();
    scene.scope.iter().all(|scope| {
        let valid = (scope.volume || scope.mute)
            && !seen.contains(&scope.player)
            && saved
                .snapshot
                .levels
                .iter()
                .filter(|level| level.player == scope.player)
                .count()
                == 1
            && saved
                .snapshot
                .levels
                .iter()
                .any(|level| level.player == scope.player && level.volume <= 100)
            && saved
                .after
                .levels
                .iter()
                .filter(|level| level.player == scope.player)
                .count()
                == 1;
        seen.push(scope.player.clone());
        valid
    })
}

/// Bind a completed scene to its native replacement receipt. Failed steps,
/// incomplete fades or changed final levels cannot publish a partial undo.
pub fn finish_scene_queue_replacement<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    pending: PendingSceneQueueUndo,
    receipt: &QueueReplacement,
    report: &SceneReport,
    at: i64,
) -> Result<QueueUndo, CoreError> {
    if !report.is_complete()
        || report.notes != pending.plan.notes
        || report
            .done
            .iter()
            .filter(|op| **op == pending.source)
            .count()
            != 1
        || pending.plan.ops.iter().any(|op| {
            matches!(op, SceneOp::Volume { .. } | SceneOp::Mute { .. }) && !report.done.contains(op)
        })
    {
        return Err(unavailable(
            "the scene did not complete all of its requested changes",
        ));
    }
    let original = pending.pending;
    let before = &original.before;
    let owner = receipt.owner.as_ref().ok_or_else(|| {
        unavailable("the scene replacement has no stable original playback proof")
    })?;
    let elapsed = u32::try_from(original.observed.elapsed().as_secs()).unwrap_or(u32::MAX);
    if original.expected.is_some()
        || receipt.autoplay != pending.restore.playing
        || receipt.coordinator != original.snapshot.coordinator
        || !original.matches_before(receipt.before.revision, &receipt.before.uris)
        || receipt.after.revision == 0
        || receipt.after.revision == receipt.before.revision
        || !before
            .playback
            .agrees(&Playback::from_owner(owner), elapsed)
    {
        return Err(unavailable(
            "the scene replacement does not own the saved queue and playback",
        ));
    }
    let mode = if receipt.in_order {
        Some(PlayMode::Normal)
    } else {
        before.playback.mode
    };
    let (after, _) = Proof::read(t, households, &receipt.coordinator, mode.is_some(), at)?;
    let started = u32::try_from(receipt.started.elapsed().as_secs()).unwrap_or(u32::MAX);
    if !selected(&after, pending.restore.playing)
        || after.revision != receipt.after.revision
        || after.uris != receipt.after.uris
        || after.binding != before.binding
        || after.levels != pending.levels
        || after.playback.mode != mode
        || after.playback.position.is_none_or(|position| {
            if receipt.autoplay {
                position > started.saturating_add(1)
            } else {
                position != 0
            }
        })
    {
        return Err(unavailable(
            "the scene's queue, playback, levels or physical group changed before logging",
        ));
    }
    let undo = QueueUndo {
        version: 3,
        snapshot: original.snapshot,
        backup: original.backup,
        after,
        scene: Some(pending.restore),
    };
    undo.validate()?;
    Ok(undo)
}

fn restores_unmute(saved: &QueueUndo) -> bool {
    saved.scene.as_ref().is_some_and(|scene| {
        scene.scope.iter().any(|scope| {
            scope.mute
                && saved
                    .snapshot
                    .levels
                    .iter()
                    .any(|level| level.player == scope.player && !level.mute)
        })
    })
}

/// Accept only the bookmark and native revision produced by reconstruction,
/// while keeping its unchanged physical rooms and levels as the next fence.
fn quiet_queue_proof<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    saved: &QueueUndo,
    before: &Proof,
    reference: &queue::QueueReference,
    at: i64,
) -> Result<Proof, CoreError> {
    let (restored, _) = Proof::read(
        t,
        households,
        saved.coordinator(),
        saved.has_play_mode(),
        at,
    )?;
    let original_uris: Vec<_> = saved
        .backup
        .entries
        .iter()
        .map(|entry| entry.uri.clone())
        .collect();
    if restored.binding != before.binding
        || restored.levels != before.levels
        || restored.revision != reference.update_id
        || restored.uris != original_uris
        || restored.playback.track != reference.track
        || restored.playback.position != Some(reference.position_secs)
        || restored.playback.mode != reference.play_mode
        || !restored.playback.metadata.is_empty()
        || !matches!(
            restored.playback.state,
            TransportState::Paused | TransportState::Stopped
        )
    {
        return Err(unavailable(
            "the silently restored queue changed before scene levels could be restored",
        ));
    }
    Ok(restored)
}

/// Rebuild the original queue in silence before raising a level or unmuting.
/// Its exact native revision then owns the level writes and final resume, so
/// an unrelated queue cannot be adopted between reconstruction and playback.
pub(super) fn restore<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    saved: &QueueUndo,
    at: i64,
    vet: &dyn Fn(&RestoreOp) -> Result<RestoreOp, String>,
) -> Result<RestoreReport, CoreError> {
    let started = Instant::now();
    let mut proof = saved.after.clone();
    proof.confirm(t, households, later(at, started))?;
    let audible = saved.starts_playback() || restores_unmute(saved);
    let capped = audible && cap_levels(t, households, &mut proof, later(at, started), vet)?;
    let owner = proof.confirm(t, households, later(at, started))?;
    let rebuilt = {
        let fenced = Fenced {
            t,
            households,
            proof: &proof,
            poisoned: Cell::new(false),
        };
        let baseline = queue_backup::Baseline {
            owner: &owner,
            revision: proof.revision,
            uris: &proof.uris,
        };
        let rebuilt = queue_backup::restore_bound_with_receipt(
            &fenced,
            households,
            saved.coordinator(),
            &saved.backup,
            false,
            Some(&baseline),
        )
        .map_err(unavailable)?;
        if fenced.poisoned.get() {
            return Err(unavailable(
                "scene queue reconstruction lost its ownership fence",
            ));
        }
        rebuilt
    };
    let reference = rebuilt.reference;
    proof = quiet_queue_proof(t, households, saved, &proof, &reference, later(at, started))?;
    let levels = restore_levels(t, households, saved, &mut proof, later(at, started), vet)?;
    if saved.starts_playback() {
        let owner = proof.confirm(t, households, later(at, started))?;
        let fenced = Fenced {
            t,
            households,
            proof: &proof,
            poisoned: Cell::new(false),
        };
        queue::restore_from(
            &fenced,
            households,
            saved.coordinator(),
            &reference,
            true,
            Some((&owner.playback, owner.mode)),
        )
        .map_err(unavailable)?;
        if fenced.poisoned.get() {
            return Err(unavailable(
                "the restored queue lost ownership before playback could resume",
            ));
        }
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
    for aspect in levels {
        if !restored.contains(&aspect) {
            restored.push(aspect);
        }
    }
    Ok(RestoreReport {
        restored,
        skipped: Vec::new(),
    })
}

/// Restore only fields the scene deliberately changed. Re-mute before changing
/// levels, and lower policy-capped volumes before any unmute. Each write keeps
/// the silently reconstructed queue/playback proof, updated only for its own exact
/// acknowledged or read-back-confirmed level change.
fn restore_levels<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    saved: &QueueUndo,
    proof: &mut Proof,
    at: i64,
    vet: &dyn Fn(&RestoreOp) -> Result<RestoreOp, String>,
) -> Result<Vec<Aspect>, CoreError> {
    let Some(scene) = &saved.scene else {
        return Ok(Vec::new());
    };
    let started = Instant::now();
    let mut ops = [Vec::new(), Vec::new(), Vec::new()];
    for scope in &scene.scope {
        let level = saved
            .snapshot
            .levels
            .iter()
            .find(|level| level.player == scope.player)
            .ok_or_else(|| unavailable("the scene's original room level is missing"))?;
        if scope.mute {
            let index = if level.mute { 0 } else { 2 };
            ops[index].push(RestoreOp::SetMute {
                player: scope.player.clone(),
                mute: level.mute,
            });
        }
        if scope.volume {
            ops[1].push(RestoreOp::SetVolume {
                player: scope.player.clone(),
                level: level.volume,
            });
        }
    }
    let mut restored = Vec::new();
    for planned in ops.into_iter().flatten() {
        let vetted = vet(&planned).map_err(unavailable)?;
        let (player, aspect) = match (&planned, &vetted) {
            (
                RestoreOp::SetVolume { player, .. },
                RestoreOp::SetVolume {
                    player: target,
                    level: capped,
                },
            ) if player == target && *capped <= 100 => (player, Aspect::Volume),
            (
                RestoreOp::SetMute { player, mute },
                RestoreOp::SetMute {
                    player: target,
                    mute: next,
                },
            ) if player == target && mute == next => (player, Aspect::Mute),
            _ => {
                return Err(unavailable(
                    "scene level restoration changed its player or operation",
                ));
            }
        };
        proof.confirm(t, households, later(at, started))?;
        let index = proof
            .levels
            .iter()
            .position(|level| level.player == *player)
            .ok_or_else(|| unavailable("the scene's room left its queue group"))?;
        let host = proof.binding.host(player)?;
        let result = match vetted {
            RestoreOp::SetVolume { level, .. } if proof.levels[index].volume != level => {
                let written = soap::set_volume(t, host, level);
                proof.levels[index].volume = level;
                Some(written)
            }
            RestoreOp::SetMute { mute, .. } if proof.levels[index].mute != mute => {
                let written = soap::set_mute(t, host, mute);
                proof.levels[index].mute = mute;
                Some(written)
            }
            _ => None,
        };
        if let Some(written) = result {
            if matches!(
                &written,
                Err(ProtoError::SoapFault { .. } | ProtoError::NotWired(_))
            ) {
                return Err(written
                    .expect_err("the write was definitely refused")
                    .into());
            }
            let confirmed = proof.confirm(t, households, later(at, started));
            match (written, confirmed) {
                (Ok(()) | Err(_), Ok(_)) => {}
                (Err(error), Err(_)) => return Err(error.into()),
                (Ok(()), Err(error)) => return Err(error),
            }
        }
        if !restored.contains(&aspect) {
            restored.push(aspect);
        }
    }
    Ok(restored)
}
