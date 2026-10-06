//! FrankenSonos core: the daemon's brain.
//!
//! Owns the authoritative in-memory model of the two households (inventory,
//! zone topology, per-group transport state), the grouping operations, and the
//! control orchestration that turns high-level intents ("play this on Jeff's
//! Office") into [`fsonos_proto`] SOAP calls against the right coordinator.
//!
//! The durable [`store`] (device cache, music-library cache, play history, DJ
//! state) is backed by fsqlite once bead FND-DEPS wires it; the trait here lets
//! the rest of the daemon be written and tested against an in-memory store.

pub mod inventory;
pub mod store;

use fsonos_types::{HouseholdId, Player, PlayerId, ZoneGroup};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error(transparent)]
    Proto(#[from] fsonos_proto::ProtoError),
    #[error("unknown player: {0}")]
    UnknownPlayer(String),
    #[error("unknown household: {0}")]
    UnknownHousehold(String),
    #[error("store error: {0}")]
    Store(String),
}

/// The authoritative snapshot of everything the daemon knows about one
/// household at a point in time.
#[derive(Debug, Default, Clone)]
pub struct HouseholdState {
    pub id: Option<HouseholdId>,
    pub players: Vec<Player>,
    pub groups: Vec<ZoneGroup>,
}

impl HouseholdState {
    /// Resolve the coordinator that owns `player` (for group-wide commands).
    #[must_use]
    pub fn coordinator_of(&self, player: &PlayerId) -> Option<&PlayerId> {
        self.groups
            .iter()
            .find(|g| g.coordinator == *player || g.members.contains(player))
            .map(|g| &g.coordinator)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_types::ZoneGroup;

    #[test]
    fn resolves_coordinator() {
        let coord = PlayerId("coord".into());
        let member = PlayerId("member".into());
        let st = HouseholdState {
            groups: vec![ZoneGroup {
                coordinator: coord.clone(),
                members: vec![coord.clone(), member.clone()],
            }],
            ..Default::default()
        };
        assert_eq!(st.coordinator_of(&member), Some(&coord));
    }
}
