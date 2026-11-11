//! Room-level actions own only the fields they can change. Their undo must
//! leave the current source, queue position, transport and other rooms alone.

use fsonos_core::HouseholdState;
use fsonos_core::actions::{self, LevelScope};
use fsonos_core::snapshot::ZoneSnapshot;
use fsonos_proto::Transport;
use fsonos_types::PlayerId;

use crate::guard::Guard;
use crate::plan::{Command, VolumeScope};
use crate::{ErrorCode, Failure, NoteCode, OutcomeDto};

pub(super) fn may_have_changed(result: &Result<OutcomeDto, Failure>) -> bool {
    match result {
        Ok(outcome) => {
            outcome.changed
                || outcome
                    .notes
                    .iter()
                    .any(|note| note.code == NoteCode::VolumeClamped)
        }
        Err(failure) => failure.code != ErrorCode::PolicyDenied,
    }
}

/// A mixed playback/group operation keeps its full snapshot semantics.
/// Ordinary levels retain only their exact physical room targets.
pub(super) fn scope(
    before: Option<String>,
    households: &[HouseholdState],
    guard: &Guard<'_>,
    commands: &[Command],
    snaps: &[ZoneSnapshot],
) -> Option<String> {
    if commands.iter().any(|command| {
        !matches!(
            command,
            Command::Volume { .. } | Command::Mute { .. } | Command::Nothing { .. }
        )
    }) {
        return before;
    }
    let mut scope = Vec::<LevelScope>::new();
    let mut add = |player: &PlayerId, volume: bool, mute: bool| {
        if let Some(saved) = scope.iter_mut().find(|saved| saved.player == *player) {
            saved.volume |= volume;
            saved.mute |= mute;
        } else {
            scope.push(LevelScope {
                player: player.clone(),
                volume,
                mute,
            });
        }
    };
    for command in commands {
        match command {
            Command::Volume {
                target,
                scope: VolumeScope::Room,
                ..
            } => add(target, true, false),
            Command::Volume {
                target,
                scope: VolumeScope::Group,
                ..
            } => {
                for room in households
                    .iter()
                    .flat_map(|house| &house.rooms)
                    .filter(|room| room.coordinator == *target)
                {
                    add(&room.primary, true, false);
                }
            }
            Command::Mute { target, mute } => {
                // Unmuting can first lower this room to the caller's cap.
                // Include that volume only when the captured level exceeds it.
                let lowers = !mute
                    && guard
                        .playback_caps(households, command)
                        .iter()
                        .any(|(room, cap, _)| {
                            room.primary == *target
                                && snaps
                                    .iter()
                                    .flat_map(|snap| &snap.levels)
                                    .any(|level| level.player == *target && level.volume > *cap)
                        });
                add(target, lowers, true);
            }
            Command::Nothing { .. } => {}
            _ => unreachable!("only room-level commands were admitted"),
        }
    }
    // A set can succeed in one group while another never supplied its prior
    // state. Preserve the existing partial-snapshot contract: undo only what
    // was captured, with the missed zones still named by the action's result.
    // A captured field whose post-action proof is missing remains in scope.
    scope.retain(|selected| {
        snaps
            .iter()
            .flat_map(|snap| &snap.levels)
            .any(|level| level.player == selected.player)
    });
    actions::with_level_scope(before, &scope)
}

/// Observe only the fields undo needs, so an unavailable queue or source
/// cannot erase a successful volume action's proof.
pub(super) fn after(
    transport: &dyn Transport,
    households: &[HouseholdState],
    before: Option<String>,
    scope: &[LevelScope],
    snaps: &[ZoneSnapshot],
    ramping: &[(PlayerId, u8)],
) -> Option<String> {
    let mut after = actions::capture_level_after(transport, households, scope);
    for room in &mut after {
        if room.volume.is_some()
            && let Some((_, target)) = ramping.iter().find(|(player, _)| *player == room.player)
        {
            room.ramp_from = snaps
                .iter()
                .flat_map(|snap| &snap.levels)
                .find(|level| level.player == room.player)
                .map(|level| level.volume);
            room.volume = Some(*target);
        }
    }
    actions::with_level_after(before, &after)
}

/// A manual override ends the fade's ownership of that room, including undo.
pub(super) fn omit(before: Option<String>, players: &[PlayerId]) -> Option<String> {
    let scope = before
        .as_deref()
        .and_then(|saved| actions::BeforeState::parse(saved).ok())
        .and_then(|saved| saved.level_scope);
    match scope {
        Some(mut scope) => {
            scope.retain(|entry| !players.contains(&entry.player));
            actions::with_level_scope(before, &scope)
        }
        None => before,
    }
}
