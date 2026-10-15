//! An absolute volume retry cannot spend the caller's step allowance twice.

use crate::execute::OutcomeDto;
use crate::failure::Failure;
use crate::guard::{Guard, Note};
use crate::plan::Command;
use crate::request::VolumeChange;
use fsonos_core::HouseholdState;
use fsonos_proto::Transport;

/// Scoped to one user command, not to a room or another caller's request.
#[derive(Default)]
pub(super) struct RetryVolume {
    ceiling: Option<u8>,
    notes: Vec<Note>,
}

impl RetryVolume {
    pub(super) fn execute<T: Transport + ?Sized>(
        &mut self,
        transport: &T,
        households: &[HouseholdState],
        guard: &Guard<'_>,
        mut command: Command,
    ) -> Result<OutcomeDto, Failure> {
        let absolute = if let Command::Volume {
            change: VolumeChange::Set(level),
            ..
        } = &mut command
        {
            if let Some(ceiling) = self.ceiling {
                *level = (*level).min(ceiling);
            }
            true
        } else {
            false
        };
        // Recheck the ordinary policy against the destination's current
        // levels. A stricter cap still wins; an old allowance is not a bypass.
        let (command, mut notes) = guard.bound(transport, households, command)?;
        if absolute {
            if let Command::Volume {
                change: VolumeChange::Set(level),
                ..
            } = &command
            {
                self.ceiling = Some(*level);
            }
            // Keep the original clamp explanation even when its write's
            // acknowledgement was lost and the retry needs no further clamp.
            for note in &notes {
                if !self.notes.contains(note) {
                    self.notes.push(note.clone());
                }
            }
            notes.clone_from(&self.notes);
        }
        let mut outcome = crate::execute::execute(transport, households, &command)?;
        notes.append(&mut outcome.notes);
        outcome.notes = notes;
        Ok(outcome)
    }
}
