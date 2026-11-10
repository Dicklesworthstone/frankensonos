//! A retry can reach a different audience than the one captured for undo.
//! Keep that fact per command, including failed attempts and parallel sets,
//! rather than attributing a recovered write to an obsolete zone snapshot.

use std::cell::Cell;

use fsonos_core::{HouseholdState, actions};

use crate::execute::OutcomeDto;
use crate::failure::{Failure, NoteCode};
use crate::plan::Command;

/// The rooms and physical players a command may change. Coordinator elections
/// and address changes do not change the audience; regrouping and rebonding do.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct Group {
    household: Option<String>,
    rooms: Vec<String>,
    players: Vec<String>,
}

/// Original snapshot ownership, observed again before every recovered attempt.
pub(super) struct Audience {
    original: Option<Vec<Group>>,
    changed: Cell<bool>,
}

impl Audience {
    pub(super) fn new(households: &[HouseholdState], command: &Command) -> Self {
        Self {
            original: groups(households, command),
            changed: Cell::new(false),
        }
    }

    pub(super) fn observe(&self, households: &[HouseholdState], command: &Command) {
        let current = groups(households, command);
        if self.original.is_none() || current.is_none() || current != self.original {
            self.changed.set(true);
        }
    }

    /// A failed write can have taken effect, so the barrier survives an error
    /// and a set that skips that error. Even a policy denial on the retry
    /// cannot prove the original attempt had no effect. A successful no-op
    /// leaves the earlier action available to undo.
    pub(super) fn unsafe_undo(&self, result: &Result<OutcomeDto, Failure>) -> bool {
        self.changed.get()
            && match result {
                Ok(outcome) => {
                    outcome.changed
                        || outcome
                            .notes
                            .iter()
                            .any(|note| note.code == NoteCode::VolumeClamped)
                }
                Err(_) => true,
            }
    }
}

/// No snapshot was taken of a newly reached audience before the write. Mark
/// the action as a boundary; neither ordinary nor forced undo may restore the
/// unrelated old group or reach past this action to an older one.
pub(super) fn protect(before: Option<String>, unsafe_undo: bool) -> Option<String> {
    if unsafe_undo {
        Some(actions::before_state_not_undoable(
            "recovered command whose room group changed before execution",
        ))
    } else {
        before
    }
}

fn groups(households: &[HouseholdState], command: &Command) -> Option<Vec<Group>> {
    let mut result = Vec::new();
    for coordinator in super::affected(households, command) {
        let mut matches = households.iter().filter_map(|household| {
            household
                .groups
                .iter()
                .find(|group| group.coordinator == coordinator)
                .map(|group| (household, group))
        });
        let (household, group) = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        let mut rooms: Vec<_> = household
            .rooms
            .iter()
            .filter(|room| room.coordinator == coordinator)
            .map(|room| room.primary.0.clone())
            .collect();
        let mut players: Vec<_> = group
            .members
            .iter()
            .map(|player| player.0.clone())
            .collect();
        if rooms.is_empty() || players.is_empty() {
            return None;
        }
        rooms.sort();
        players.sort();
        result.push(Group {
            household: household.id.as_ref().map(|id| id.0.clone()),
            rooms,
            players,
        });
    }
    result.sort();
    result.dedup();
    Some(result)
}
