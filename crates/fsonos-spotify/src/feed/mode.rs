//! Queue-order control for whole-work starts and live mode changes.

use fsonos_core::{control, playback::PlayerPlayback};
use fsonos_proto::{Transport, control::set_normal_play_mode};

use super::{FeedError, QueueFeed, Speakers, host_of};

/// Some players reject SetPlayMode while a stream, not the queue, is active.
/// The core starts the queue, sets NORMAL, seeks, then plays; any failed
/// prerequisite prevents the later stages. Ordinary queue starts are unchanged.
pub(super) fn start<T: Transport + ?Sized>(
    at: &Speakers<'_, T>,
    position: u32,
) -> Result<(), FeedError> {
    control::play_queue_from_in_order(at.transport, at.households, at.coordinator, position)?;
    Ok(())
}

/// A mode-only event can require restoring sequential playback. Returning
/// false leaves the rest of this event unprocessed.
pub(super) fn maintain<T: Transport + ?Sized>(
    feed: &mut QueueFeed,
    at: &Speakers<'_, T>,
    playback: &PlayerPlayback,
) -> Result<bool, FeedError> {
    if playback
        .play_mode
        .as_deref()
        .is_some_and(|mode| mode.trim() != "NORMAL")
    {
        let result = set_normal_play_mode(at.transport, host_of(at)?);
        if result.is_err() {
            feed.queue_len = None;
        }
        result?;
    }
    Ok(true)
}
