//! A survey may read in the background; only the reconciler commits it.

use super::{HealthBoard, Reconciler, RefreshReport};
use crate::inventory::{self, DISCOVERY_WAIT, Survey};
use crate::{CoreError, HouseholdState};
use fsonos_proto::Transport;
use fsonos_types::PlayerId;
use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Instant;

/// Owned discovery hints, with no subscriptions or mutable live state.
#[derive(Clone)]
pub(crate) struct InventoryPlan {
    known: Vec<HouseholdState>,
    seeds: Vec<IpAddr>,
    topology: Arc<()>,
    health: HealthBoard,
}

impl InventoryPlan {
    pub(crate) fn read<T: Transport + ?Sized>(&self, transport: &T) -> Result<Survey, CoreError> {
        inventory::survey_known(transport, &self.seeds, &self.known, DISCOVERY_WAIT)
    }

    pub(crate) fn current(&self, rec: &Reconciler) -> bool {
        Arc::ptr_eq(&self.topology, &rec.inventory_epoch)
    }

    /// A player heard since dispatch cannot be removed or relocated by an
    /// older survey. Ordinary volume/transport events do not reject a survey
    /// which still agrees about that player's identity and address.
    fn contradicts_later_event(&self, rec: &Reconciler, survey: &Survey) -> bool {
        rec.households
            .iter()
            .flat_map(|h| &h.players)
            .any(|player| {
                let before = self.health.of(&player.id).and_then(|h| h.last_ok);
                let after = rec.health.of(&player.id).and_then(|h| h.last_ok);
                after.is_some()
                    && before != after
                    && !survey.households.iter().any(|house| {
                        house.player(&player.id).is_some_and(|found| {
                            found.ip == player.ip && found.generation == player.generation
                        })
                    })
                    && !rec.households.iter().any(|house| {
                        house.player(&player.id).is_some() && retain_household(survey, house)
                    })
            })
    }
}

fn retain_household(survey: &Survey, previous: &HouseholdState) -> bool {
    survey.rejected_topology.iter().any(|(player, hint)| {
        previous.player(player).is_some()
            || hint
                .as_ref()
                .is_some_and(|id| previous.id.as_ref() == Some(id))
    }) && !survey.households.iter().any(|fresh| {
        inventory::same_household(previous, fresh)
            || previous
                .players
                .iter()
                .any(|old| fresh.players.iter().any(|player| player.ip == old.ip))
    })
}

impl Reconciler {
    /// Failed topology admission is not a confirmed departure. Keep only an
    /// affected live household which no accepted observation replaced or
    /// contradicted; healthy households still advance independently. Cached
    /// discovery hints are never resurrected as live rooms by this path.
    pub(super) fn retain_rejected_households(&self, survey: &mut Survey) -> HashSet<PlayerId> {
        let mut retained = HashSet::new();
        let mut missing = HashSet::new();
        for previous in &self.households {
            if !retain_household(survey, previous) {
                continue;
            }
            retained.extend(previous.players.iter().map(|player| player.id.clone()));
            missing.extend(
                previous
                    .rooms
                    .iter()
                    .flat_map(|room| room.missing.iter().cloned()),
            );
            survey.households.push(previous.clone());
        }
        if !retained.is_empty() {
            survey
                .vanished
                .retain(|gone| !retained.contains(&gone.uuid));
            // Retain bonded absence evidence with its old room, without
            // copying boot observations or claiming these players answered.
            let mut vanished: HashSet<_> = survey
                .vanished
                .iter()
                .map(|gone| gone.uuid.clone())
                .collect();
            for gone in &self.vanished {
                if missing.contains(&gone.uuid)
                    && !survey
                        .households
                        .iter()
                        .any(|house| house.player(&gone.uuid).is_some())
                    && vanished.insert(gone.uuid.clone())
                {
                    survey.vanished.push(gone.clone());
                }
            }
            inventory::sort_households(&mut survey.households);
        }
        retained
    }

    pub(crate) fn inventory_plan(&mut self, seeds: &[IpAddr]) -> InventoryPlan {
        self.discovery.observe(&self.households);
        InventoryPlan {
            known: self.discovery.households().to_vec(),
            seeds: seeds.to_vec(),
            topology: Arc::clone(&self.inventory_epoch),
            health: self.health.clone(),
        }
    }

    /// Invalidate even an equal or returning topology, without a wrapping
    /// counter. Pending plans keep the old allocation alive until discarded.
    pub(crate) fn invalidate_inventory(&mut self) {
        self.inventory_epoch = Arc::new(());
    }

    /// Stale evidence is not a network failure, nor a reason to spin. Keep
    /// live state and retry within the existing subscription recovery window.
    pub(crate) fn obsolete_inventory(&mut self, now: Instant) {
        self.schedule.succeeded(now);
        self.schedule.retry_within(now, crate::events::RETRY);
    }

    pub(crate) fn finish_inventory(
        &mut self,
        plan: &InventoryPlan,
        surveyed: Result<Survey, CoreError>,
        now: Instant,
    ) -> Result<Option<RefreshReport>, CoreError> {
        if !plan.current(self)
            || surveyed
                .as_ref()
                .is_ok_and(|survey| plan.contradicts_later_event(self, survey))
        {
            self.obsolete_inventory(now);
            return Ok(None);
        }
        // Renewals and notifications continue during the read. Preserve their
        // newer health evidence rather than resetting it from old probe results.
        let newer: Vec<_> = self
            .health
            .players
            .iter()
            .filter(|(player, health)| plan.health.of(player) != Some(*health))
            .map(|(player, health)| (player.clone(), health.clone()))
            .collect();
        let result = self.apply_inventory(surveyed, now);
        for (player, health) in newer {
            if self
                .households
                .iter()
                .any(|house| house.player(&player).is_some())
            {
                self.health.players.insert(player, health);
            }
        }
        result.map(Some)
    }
}

#[cfg(test)]
mod tests;
