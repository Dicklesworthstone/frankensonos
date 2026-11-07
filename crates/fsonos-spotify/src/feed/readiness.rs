//! Read-only admission before a scheduled or scene-driven DJ start.

use super::{
    FeedError, PickContext, Planning, PlayRecord, QueueFeed, Speakers, Store, Transport, enqueue,
    host_of, pick_next, read_render_params,
};

impl QueueFeed {
    /// Check the reads and selections a start needs, without consuming any
    /// music or changing this feed. Retained works are reconciled in a clone,
    /// so a complete existing set can resume even if the library is empty.
    /// This is a preflight, not a reservation: start repeats its normal
    /// checks because another controller can change a speaker afterward.
    pub fn start_ready<T: Transport + ?Sized, S: Store + ?Sized>(
        &self,
        at: &Speakers<'_, T>,
        plan: Planning<'_>,
        store: &S,
    ) -> Result<(), FeedError> {
        let mut preview = self.clone();
        if preview.active {
            preview.resync(at)?;
        } else {
            preview.release();
        }
        let partial = preview.partial_to_finish();
        let count = (1 + preview.lookahead()).saturating_sub(preview.queued.len());
        let mut additional = partial.map_or(0, |index| {
            preview.queued[index]
                .movements
                .iter()
                .filter(|movement| !movement.added)
                .count()
        });
        if count == 0 && partial.is_none() {
            return Ok(());
        }
        if count > 0 && plan.pool.is_empty() {
            return Err(FeedError::EmptyPool);
        }
        // Favorites reads can fail even when cached parameters exist. Keep
        // exactly the same rule as the real start instead of masking a fault.
        read_render_params(at, store)?;
        if count > 0 {
            let mut history = preview.history(store, plan.now)?;
            let elsewhere = preview.elsewhere(store, &plan)?;
            for _ in 0..count {
                let context = PickContext {
                    history: &history,
                    elsewhere: &elsewhere,
                    now: Some(plan.now),
                    local_hour: plan.local_hour,
                    energy_target: None,
                    steer: plan.steer,
                    feedback: plan.feedback,
                };
                let work = pick_next(plan.pool, &context, &preview.config, &mut preview.rng)
                    .ok_or_else(|| {
                        plan.pool
                            .why_nothing_plays(plan.steer, Some(plan.now))
                            .map_or(FeedError::EmptyPool, FeedError::NothingAllowed)
                    })?;
                additional = additional
                    .checked_add(work.movements.len())
                    .ok_or_else(|| {
                        fsonos_proto::ProtoError::Malformed(
                            "the DJ start exceeds the queue verification limit".into(),
                        )
                    })?;
                history.extend(
                    work.movements.iter().map(|movement| {
                        PlayRecord::at(movement.track.source_uri.clone(), plan.now)
                    }),
                );
            }
        }
        // Scenes may appoint a player that is currently another group's
        // member. Its future queue/owner can only be proved after regrouping.
        // A current coordinator can check these prerequisites before admission.
        if at.households.iter().any(|household| {
            household
                .groups
                .iter()
                .any(|group| group.coordinator == *at.coordinator)
        }) {
            enqueue::ready(at.transport, host_of(at)?, at.coordinator, additional)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
