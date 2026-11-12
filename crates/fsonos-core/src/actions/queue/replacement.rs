//! A queue replacement changes playback deliberately. Its undo proof must
//! bind the original contents to the successful staged mutation, separately
//! from the stricter playback-preserving removal and movement finalizer.

use super::proof::{Playback, Proof};
use super::{PendingQueueUndo, QueueUndo, unavailable};
use crate::control::QueueReplacement;
use crate::scenes::{MAX_QUEUE_BACKUP_BYTES, MAX_QUEUE_BACKUP_ENTRIES};
use crate::{CoreError, HouseholdState};
use fsonos_proto::Transport;
use fsonos_types::{PlayMode, PlayerId, TransportState};

impl PendingQueueUndo {
    /// Account for the exact policy reductions successfully written before
    /// Play. A cap cannot introduce a player, raise its volume or change mute.
    /// Undo preserves these levels and remains subject to current policy.
    pub fn volume_caps(mut self, caps: &[(PlayerId, u8)]) -> Result<Self, CoreError> {
        let mut seen = Vec::new();
        for (player, cap) in caps {
            if seen.contains(player) {
                return Err(unavailable("a playback cap names a room more than once"));
            }
            seen.push(player.clone());
            let level = self
                .before
                .levels
                .iter_mut()
                .find(|level| level.player == *player)
                .ok_or_else(|| unavailable("a playback cap names a different physical room"))?;
            if *cap > level.volume {
                return Err(unavailable(
                    "a playback cap cannot raise the saved room volume",
                ));
            }
            level.volume = *cap;
        }
        Ok(self)
    }

    /// Confirm the saved queue, playback, physical group and exact capped
    /// levels immediately before executing a replacement. Failure affects
    /// undo eligibility; it is not permission to reconstruct another queue.
    pub fn confirm<T: Transport + ?Sized>(
        &self,
        t: &T,
        households: &[HouseholdState],
        at: i64,
    ) -> Result<(), CoreError> {
        self.before.confirm(t, households, at).map(|_| ())
    }
}

pub(super) fn valid_queue(after: &Proof) -> bool {
    !after.uris.is_empty()
        && after.uris.len() <= MAX_QUEUE_BACKUP_ENTRIES
        && after
            .uris
            .iter()
            .try_fold(0_usize, |bytes, uri| {
                if uri.is_empty() {
                    None
                } else {
                    bytes.checked_add(uri.len())
                }
            })
            .is_some_and(|bytes| bytes <= MAX_QUEUE_BACKUP_BYTES)
        && after.playback.source == format!("x-rincon-queue:{}#0", after.binding.coordinator.0)
        && after.playback.metadata.is_empty()
        && after.playback.track == 1
}

pub(super) fn valid_result(after: &Proof) -> bool {
    valid_queue(after) && after.playback.state == TransportState::Playing
}

/// Finish bounded queue undo only after this request successfully replaced
/// and started its queue. A receipt from a different baseline, a partial or
/// failed attempt, a changed owner or an unreadable result leaves a barrier.
/// The original selected occurrence, time, mode and quiet intent are retained.
pub fn finish_queue_replacement<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    pending: PendingQueueUndo,
    receipt: &QueueReplacement,
    at: i64,
) -> Result<QueueUndo, CoreError> {
    let before = &pending.before;
    let owner = receipt
        .owner
        .as_ref()
        .ok_or_else(|| unavailable("the replacement has no stable original playback proof"))?;
    let elapsed = u32::try_from(pending.observed.elapsed().as_secs()).unwrap_or(u32::MAX);
    if pending.expected.is_some()
        || !receipt.autoplay
        || receipt.coordinator != pending.snapshot.coordinator
        || !pending.matches_before(receipt.before.revision, &receipt.before.uris)
        || receipt.after.revision == 0
        || receipt.after.revision == receipt.before.revision
        || !before
            .playback
            .agrees(&Playback::from_owner(owner), elapsed)
    {
        return Err(unavailable(
            "the completed replacement does not own the saved queue and playback",
        ));
    }
    let mode = if receipt.in_order {
        Some(PlayMode::Normal)
    } else {
        before.playback.mode
    };
    let (after, _) = Proof::read(t, households, &receipt.coordinator, mode.is_some(), at)?;
    let started = u32::try_from(receipt.started.elapsed().as_secs()).unwrap_or(u32::MAX);
    if !valid_result(&after)
        || after.revision != receipt.after.revision
        || after.uris != receipt.after.uris
        || after.binding != before.binding
        || after.levels != before.levels
        || after.playback.mode != mode
        || after
            .playback
            .position
            .is_none_or(|position| position > started.saturating_add(1))
    {
        return Err(unavailable(
            "the replacement queue or playback changed before logging",
        ));
    }
    let undo = QueueUndo {
        version: 2,
        snapshot: pending.snapshot,
        backup: pending.backup,
        after,
        scene: None,
    };
    undo.validate()?;
    Ok(undo)
}
