//! Tone undo owns only the fields changed on the original room player.

use super::{LoggedAction, ToneBefore, levels, room_name};
use crate::control::ToneChange;
use crate::{CoreError, HouseholdState};
use fsonos_proto::{Transport, control as soap};
use fsonos_types::PlayerId;

/// Observe only the tone fields a write touched. A missing player or failed
/// read stays unknown; unrelated settings and playback are never read.
pub fn capture_tone_after<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    player: &PlayerId,
    scope: ToneChange,
) -> ToneChange {
    let Some(player) = levels::player(households, player) else {
        return ToneChange::default();
    };
    ToneChange {
        bass: scope.bass.and_then(|_| soap::get_bass(t, player.ip).ok()),
        treble: scope
            .treble
            .and_then(|_| soap::get_treble(t, player.ip).ok()),
        loudness: scope
            .loudness
            .and_then(|_| soap::get_loudness(t, player.ip).ok()),
    }
}

pub(super) fn restore_change(was: &ToneBefore) -> ToneChange {
    let Some(scope) = was.scope else {
        return ToneChange::all(was.tone);
    };
    ToneChange {
        bass: scope.bass.map(|_| was.tone.bass),
        treble: scope.treble.map(|_| was.tone.treble),
        loudness: scope.loudness.map(|_| was.tone.loudness),
    }
}

fn verified<T: PartialEq>(expected: Option<T>, now: Option<T>) -> bool {
    expected.is_some() && expected == now
}

/// Validate every scoped tone before any part of a compound undo writes.
/// Force may override a changed value, never the identity or saved scope.
pub(super) fn validate<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    target: &LoggedAction,
    tones: &[ToneBefore],
    force: bool,
) -> Result<(), CoreError> {
    let mut changes = Vec::new();
    for was in tones {
        let Some(scope) = was.scope else { continue };
        let name = room_name(households, &was.player);
        if levels::player(households, &was.player).is_none() {
            return Err(CoreError::NotUndoable(format!(
                "{name} is no longer a unique room primary; tone undo cannot restore a different player"
            )));
        }
        let restore = restore_change(was);
        if restore
            .bass
            .into_iter()
            .chain(restore.treble)
            .any(|value| !(-10..=10).contains(&value))
        {
            return Err(CoreError::NotUndoable(format!(
                "{name}'s captured tone is invalid; it cannot be restored"
            )));
        }
        if force || scope.is_empty() {
            continue;
        }
        let expected = was.after.unwrap_or_default();
        let now = capture_tone_after(t, households, &was.player, scope);
        for (field, selected, same) in [
            (
                "bass",
                scope.bass.is_some(),
                expected.bass == scope.bass && verified(expected.bass, now.bass),
            ),
            (
                "treble",
                scope.treble.is_some(),
                expected.treble == scope.treble && verified(expected.treble, now.treble),
            ),
            (
                "loudness",
                scope.loudness.is_some(),
                expected.loudness == scope.loudness && verified(expected.loudness, now.loudness),
            ),
        ] {
            if selected && !same {
                changes.push(format!("{name}'s {field} changed or could not be verified"));
            }
        }
    }
    if changes.is_empty() {
        return Ok(());
    }
    Err(CoreError::NotUndoable(format!(
        "#{} ({}) is left as it is: {}; read the affected tone settings, or use `fsonos undo --force` to restore only the fields this action changed",
        target.id,
        target.action.intent,
        changes.join("; ")
    )))
}
