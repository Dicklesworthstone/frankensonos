//! An absolute volume retry cannot spend the caller's step allowance twice.

use crate::execute::Executed;
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
    receipt: bool,
}

impl RetryVolume {
    pub(super) fn new(receipt: bool) -> Self {
        Self {
            receipt,
            ..Self::default()
        }
    }

    pub(super) fn execute<T: Transport + ?Sized>(
        &mut self,
        transport: &T,
        households: &[HouseholdState],
        guard: &Guard<'_>,
        mut command: Command,
    ) -> Result<Executed, Failure> {
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
        let mut executed = if self.receipt {
            crate::execute::execute_with_receipt(transport, households, &command)?
        } else {
            crate::execute::execute(transport, households, &command)?.into()
        };
        notes.append(&mut executed.outcome.notes);
        executed.outcome.notes = notes;
        Ok(executed)
    }
}
