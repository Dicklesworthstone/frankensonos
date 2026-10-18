//! Recover callback loss without closing the listener which also serves media.

use super::{Engine, LiveEvent, Model, progress};
use std::sync::atomic::Ordering;
use std::time::Instant;

impl Engine {
    /// An ordered inbox-loss marker follows every retained raw callback. It
    /// says nothing about speaker health and is not a stopped HTTP listener.
    pub(super) fn callback_loss(&self, model: &mut Model) -> Vec<LiveEvent> {
        if self.stop.load(Ordering::Acquire) {
            return Vec::new();
        }
        tracing::warn!(
            "callback backlog lost state; refreshing without closing the media listener"
        );
        let now = Instant::now();
        model.rec.subscriptions.callbacks_lost(now);
        model.rec.invalidate_inventory();
        model.rec.schedule.due_now(now);

        // Reads begun before the loss boundary may predate an unseen change,
        // even when their visible identity and observation stamps still match.
        // Reuse the same cancellation fence as a reboot, without changing the
        // health board or discarding any accepted playback or topology.
        let players: Vec<_> = model
            .rec
            .households
            .iter()
            .flat_map(|house| &house.players)
            .map(|player| player.id.clone())
            .collect();
        progress::invalidate_rebooted(model, &players);
        progress::request_survey(model);
        vec![LiveEvent::Resync]
    }
}

#[cfg(test)]
mod tests;
