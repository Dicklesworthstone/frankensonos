//! Consume a saved exhausted-queue retry before its speaker writes begin.

use super::{SpotifyDj, State, retry, saved};
use fsonos_core::playback::PlayerPlayback;
use fsonos_types::PlayerId;
use std::time::Instant;

/// A definitely refused attempt may save its cursor again afterward. A lost
/// reply or process exit must leave a revision that cannot replay that attempt.
/// Failed persistence defers the attempt with the usual bounded retry delay.
pub(super) fn prepare(
    dj: &SpotifyDj,
    state: &mut State,
    coordinator: &PlayerId,
    playback: &PlayerPlayback,
) -> bool {
    // Even a renderer-error recovery or a newer playing/paused observation
    // can retire a previously saved cursor. Consume it before any such path
    // can write, so a failed later save cannot revive the older permission.
    let Some(path) = &dj.feeds_file else {
        return true;
    };
    let Some(text) = saved::encode_before_continuation(
        &state.feeds,
        &state.handover,
        &state.starters,
        coordinator,
    ) else {
        return true;
    };
    if text == state.saved {
        return true;
    }
    if let Err(error) = saved::write(path, &text) {
        let retry = retry::Retry::failed(
            state.refill_retries.get(coordinator),
            playback,
            Instant::now(),
        );
        state.problems.insert(
            coordinator.clone(),
            format!(
                "The DJ could not save its queue continuation before playback: {error}; \
                 retrying in {} s",
                retry.delay().as_secs()
            ),
        );
        state.refill_retries.insert(coordinator.clone(), retry);
        if let Some(listening) = state.listening.get_mut(coordinator) {
            listening.interrupt();
        }
        tracing::warn!(%error, "the DJ defers its queue continuation until it can save");
        return false;
    }
    state.saved = text;
    true
}

#[cfg(test)]
mod tests;
