//! Level-only undo keeps music, grouping and unrelated rooms independent.

use super::{BeforeState, LoggedAction, ZoneOutcomes, room_name};
use crate::snapshot::{Aspect, MemberLevel, RestoreOp, RestoreReport};
use crate::{CoreError, HouseholdState};
use fsonos_proto::{Transport, control as soap};
use fsonos_types::{Player, PlayerId};
use serde::{Deserialize, Serialize};

/// The exact physical player and fields a level-only action may put back.
/// A missing scope in an older action preserves its full snapshot behavior.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LevelScope {
    pub player: PlayerId,
    pub volume: bool,
    pub mute: bool,
}

/// Observed fields after a scoped action. None means unreadable, never an
/// invented level. A later undo requires every field selected by its scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LevelAfter {
    pub player: PlayerId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mute: Option<bool>,
    /// A device-speed ramp may still be anywhere between here and `volume`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ramp_from: Option<u8>,
}

fn normalized(scope: &[LevelScope]) -> Vec<LevelScope> {
    let mut out: Vec<LevelScope> = Vec::new();
    for next in scope.iter().filter(|s| s.volume || s.mute) {
        if let Some(prior) = out.iter_mut().find(|s| s.player == next.player) {
            prior.volume |= next.volume;
            prior.mute |= next.mute;
        } else {
            out.push(next.clone());
        }
    }
    out
}

fn with_field(before: Option<String>, key: &str, content: serde_json::Value) -> Option<String> {
    let before = before?;
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&before) else {
        return Some(before);
    };
    if value.is_array() {
        value = serde_json::json!({ "zones": value });
    }
    let Some(fields) = value.as_object_mut() else {
        return Some(before);
    };
    fields.insert(key.into(), content);
    Some(value.to_string())
}

/// Restrict a new action's captured zones to these level fields. Duplicate
/// players merge; an explicit empty scope never falls back to a full restore.
/// Unreadable prior values remain an undo refusal, not a fabricated snapshot.
#[must_use]
pub fn with_level_scope(before: Option<String>, scope: &[LevelScope]) -> Option<String> {
    with_field(before, "level_scope", serde_json::json!(normalized(scope)))
}

/// Retain proof for a scoped action, including an empty or incomplete read.
#[must_use]
pub fn with_level_after(before: Option<String>, after: &[LevelAfter]) -> Option<String> {
    with_field(before, "level_after", serde_json::json!(after))
}

/// A physical identity must remain unique. Never resolve a saved room name or
/// follow its current coordinator to another player during a level restore.
fn player<'a>(households: &'a [HouseholdState], id: &PlayerId) -> Option<&'a Player> {
    let mut matches = households.iter().filter_map(|h| h.player(id));
    let found = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    let mut rooms = households
        .iter()
        .flat_map(|h| &h.rooms)
        .filter(|r| r.players.contains(id));
    let room = rooms.next()?;
    (room.primary == *id && rooms.next().is_none()).then_some(found)
}

/// Read only selected level fields, without AVTransport, queue, topology, or
/// the other field. Missing players and failed reads retain explicit unknowns.
pub fn capture_level_after<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    scope: &[LevelScope],
) -> Vec<LevelAfter> {
    normalized(scope)
        .iter()
        .map(|scope| {
            let at = player(households, &scope.player);
            LevelAfter {
                player: scope.player.clone(),
                volume: scope
                    .volume
                    .then(|| at.and_then(|p| soap::get_volume(t, p.ip).ok()))
                    .flatten(),
                mute: scope
                    .mute
                    .then(|| at.and_then(|p| soap::get_mute(t, p.ip).ok()))
                    .flatten(),
                ramp_from: None,
            }
        })
        .collect()
}

fn prior<'a>(
    before: &'a BeforeState,
    scope: &LevelScope,
) -> Result<(&'a PlayerId, &'a MemberLevel), CoreError> {
    let mut levels = before.zones.iter().flat_map(|zone| {
        zone.levels
            .iter()
            .filter(|level| level.player == scope.player)
            .map(move |level| (&zone.coordinator, level))
    });
    let Some(first) = levels.next() else {
        return Err(CoreError::NotUndoable(format!(
            "no captured prior level for {}; the scoped action cannot restore it",
            scope.player.0
        )));
    };
    if levels.any(|next| next != first) {
        return Err(CoreError::NotUndoable(format!(
            "conflicting captured prior levels for {}; the scoped action cannot restore it",
            scope.player.0
        )));
    }
    if scope.volume && first.1.volume > 100 {
        return Err(CoreError::NotUndoable(format!(
            "invalid captured prior volume for {}",
            scope.player.0
        )));
    }
    Ok(first)
}

pub(super) fn validate(
    households: &[HouseholdState],
    before: &BeforeState,
) -> Result<(), CoreError> {
    if let Some(scope) = &before.level_scope {
        for scope in normalized(scope) {
            prior(before, &scope)?;
            if player(households, &scope.player).is_none() {
                return Err(CoreError::NotUndoable(format!(
                    "player {} is no longer a unique room primary; the scoped action cannot restore a different player",
                    scope.player.0
                )));
            }
        }
    }
    Ok(())
}

fn volume_matches(was: &LevelAfter, now: Option<u8>) -> bool {
    let (Some(expected), Some(now)) = (was.volume, now) else {
        return false;
    };
    now == expected
        || was
            .ramp_from
            .is_some_and(|from| (from.min(expected)..=from.max(expected)).contains(&now))
}

pub(super) fn observe<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    target: &LoggedAction,
    before: &BeforeState,
    force: bool,
) -> Result<Vec<LevelAfter>, CoreError> {
    let scope = normalized(before.level_scope.as_deref().unwrap_or_default());
    let now = capture_level_after(t, households, &scope);
    if !force {
        refuse_if_changed(households, target, before, &scope, &now)?;
    }
    Ok(now)
}

fn refuse_if_changed(
    households: &[HouseholdState],
    target: &LoggedAction,
    before: &BeforeState,
    scope: &[LevelScope],
    now: &[LevelAfter],
) -> Result<(), CoreError> {
    let mut changes = Vec::new();
    for scope in scope {
        let name = room_name(households, &scope.player);
        let mut observed = before
            .level_after
            .iter()
            .filter(|p| p.player == scope.player);
        let Some(was) = observed.next() else {
            changes.push(format!("{name}'s post-action level was not observed"));
            continue;
        };
        if observed.next().is_some() {
            changes.push(format!("{name}'s post-action level is ambiguous"));
            continue;
        }
        let is = now.iter().find(|p| p.player == scope.player);
        if scope.volume && !volume_matches(was, is.and_then(|p| p.volume)) {
            changes.push(match (was.volume, is.and_then(|p| p.volume)) {
                (Some(from), Some(to)) => format!("{name}'s volume {from} -> {to}"),
                _ => format!("{name}'s volume could not be verified"),
            });
        }
        if scope.mute && (was.mute.is_none() || was.mute != is.and_then(|p| p.mute)) {
            changes.push(match (was.mute, is.and_then(|p| p.mute)) {
                (Some(_), Some(true)) => format!("{name} muted"),
                (Some(_), Some(false)) => format!("{name} unmuted"),
                _ => format!("{name}'s mute could not be verified"),
            });
        }
    }
    if changes.is_empty() {
        return Ok(());
    }
    Err(CoreError::NotUndoable(format!(
        "#{} ({}) is left as it is: {}; read the affected levels and retry, or use `fsonos undo --force` to restore them anyway",
        target.id,
        target.action.intent,
        changes.join("; ")
    )))
}

pub(super) fn restore<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    before: &BeforeState,
    vet: &dyn Fn(&RestoreOp) -> Result<RestoreOp, String>,
) -> ZoneOutcomes {
    let mut zones: Vec<(PlayerId, RestoreReport)> = Vec::new();
    let mut failures = Vec::new();
    for scope in normalized(before.level_scope.as_deref().unwrap_or_default()) {
        let (coordinator, level) = match prior(before, &scope) {
            Ok(prior) => prior,
            Err(error) => {
                failures.push((scope.player.clone(), error.to_string()));
                continue;
            }
        };
        let index = zones
            .iter()
            .position(|(p, _)| p == coordinator)
            .unwrap_or_else(|| {
                zones.push((coordinator.clone(), RestoreReport::default()));
                zones.len() - 1
            });
        let report = &mut zones[index].1;
        let mut ops = Vec::new();
        let mute = || {
            (
                Aspect::Mute,
                RestoreOp::SetMute {
                    player: scope.player.clone(),
                    mute: level.mute,
                },
            )
        };
        // Undoing a capped unmute first restores silence, then the old level.
        // In the other direction, restore the capped level before unmuting.
        if scope.mute && level.mute {
            ops.push(mute());
        }
        if scope.volume {
            ops.push((
                Aspect::Volume,
                RestoreOp::SetVolume {
                    player: scope.player.clone(),
                    level: level.volume,
                },
            ));
        }
        if scope.mute && !level.mute {
            ops.push(mute());
        }
        let mut remute_failed = false;
        for (aspect, planned) in ops {
            let result = if aspect == Aspect::Volume && remute_failed {
                Err("not attempted because the preceding mute restoration failed".into())
            } else {
                vet(&planned).and_then(|op| execute(t, households, &planned, op))
            };
            match result {
                Ok(()) if !report.skipped.iter().any(|(a, _)| *a == aspect) => {
                    if !report.restored.contains(&aspect) {
                        report.restored.push(aspect);
                    }
                }
                Ok(()) => {}
                Err(why) => {
                    remute_failed |= aspect == Aspect::Mute && level.mute;
                    report.restored.retain(|a| *a != aspect);
                    report.skipped.push((aspect, format!("{planned:?}: {why}")));
                }
            }
        }
    }
    (zones, failures)
}

fn execute<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    planned: &RestoreOp,
    vetted: RestoreOp,
) -> Result<(), String> {
    match (planned, vetted) {
        (
            RestoreOp::SetVolume {
                player: expected, ..
            },
            RestoreOp::SetVolume {
                player: target,
                level,
            },
        ) if *expected == target && level <= 100 => {
            let at = player(households, &target)
                .ok_or_else(|| format!("player {} is unavailable or ambiguous", target.0))?;
            soap::set_volume(t, at.ip, level).map_err(|e| e.to_string())
        }
        (
            RestoreOp::SetMute {
                player: expected, ..
            },
            RestoreOp::SetMute {
                player: target,
                mute,
            },
        ) if *expected == target => {
            let at = player(households, &target)
                .ok_or_else(|| format!("player {} is unavailable or ambiguous", target.0))?;
            soap::set_mute(t, at.ip, mute).map_err(|e| e.to_string())
        }
        _ => Err("level restore policy changed its player or operation".into()),
    }
}
