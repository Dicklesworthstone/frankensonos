//! FrankenSonos core: the daemon's brain.
//!
//! Owns the authoritative in-memory model of the two households (inventory,
//! zone topology, per-group transport state), the grouping operations, and the
//! control orchestration that turns high-level intents ("play this in the
//! Living Room") into [`fsonos_proto`] SOAP calls against the right coordinator.
//!
//! * [`topology`] folds ZoneGroupTopology snapshots into groups and [`Room`]s.
//! * [`inventory`] classifies players (S1/S2) from their device descriptions.
//! * [`rooms`] resolves what a person or agent types to a [`ControlTarget`].
//! * [`playback`] keeps live playback state from GENA events.
//! * [`events`] keeps GENA subscriptions alive and routes their NOTIFYs.
//! * [`reconcile`] refreshes the inventory on a schedule and tracks player health.
//! * [`heal`] retries a command once after a player moved or its coordinator changed.
//! * [`live`] keeps the model current in the background: surveys, events, health.
//! * [`favorites`] lists, finds, and plays Sonos favorites.
//! * [`search`] ranks the cached library and favorites against a query.
//! * [`control`] carries out a player-addressed command over a
//!   [`fsonos_proto::Transport`].
//! * [`actions`] logs every mutating request and undoes the last one.
//! * [`announce`] plays announcements and chimes, then puts the music back.
//! * [`scenes`] saves named house states and applies them with the fewest changes.
//! * [`schedule`] parses schedules and decides when each runs, across DST changes.
//! * [`sleep`] runs sleep timers: fade out, pause, volumes back; the player's own timer as backstop.
//! * [`doctor`] runs diagnostic checks and reports named failures with fixes.
//! * [`policy`] bounds what each client may do and how loud (caps, quiet
//!   hours, tool allowlists); [`clock`] makes its time testable.
//!
//! The durable [`store`] (device cache, music-library cache, play history, DJ
//! state) is backed by fsqlite once bead FND-DEPS wires it; the trait here lets
//! the rest of the daemon be written and tested against an in-memory store.

pub mod actions;
pub mod announce;
pub mod clock;
pub mod control;
pub mod doctor;
pub mod events;
pub mod fade;
pub mod favorites;
pub mod grouping;
pub mod heal;
pub mod inventory;
pub mod live;
pub mod moving;
pub mod playback;
pub mod policy;
pub mod reconcile;
pub mod rooms;
pub mod scenes;
pub mod schedule;
pub mod search;
pub mod sleep;
pub mod snapshot;
pub mod store;
pub mod topology;

pub use rooms::{ControlTarget, known_rooms, resolve_room};
pub use topology::Room;

use fsonos_types::{Generation, HouseholdId, Player, PlayerId, ZoneGroup};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error(transparent)]
    Proto(#[from] fsonos_proto::ProtoError),
    #[error("unknown player: {0}")]
    UnknownPlayer(String),
    #[error("unknown household: {0}")]
    UnknownHousehold(String),
    #[error("unknown room {name:?}; known rooms: {}", list(.known))]
    UnknownRoom { name: String, known: Vec<String> },
    #[error("room {name:?} is ambiguous; use one of: {}", list(.candidates))]
    AmbiguousRoom {
        name: String,
        candidates: Vec<String>,
    },
    #[error("store error: {0}")]
    Store(String),
    #[error("this household has no Spotify track favorite to learn render parameters from")]
    NoSpotifyFavorite,
    /// The renderer refused a Spotify render with UPnP 800, the parameters
    /// were relearned from the household's favorites and the render retried
    /// once, and it still failed. Remedy: re-link Spotify in that
    /// household's Sonos app (or remove and re-add a Spotify track
    /// favorite) and retry.
    #[error(
        "Spotify render parameters are stale: relearned and retried once, \
             the player still refuses the render with UPnP 800"
    )]
    RenderParamsStale,
}

fn list(items: &[String]) -> String {
    if items.is_empty() {
        "(none discovered yet)".to_string()
    } else {
        items.join(", ")
    }
}

/// The authoritative snapshot of everything the daemon knows about one
/// household at a point in time.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HouseholdState {
    pub id: Option<HouseholdId>,
    /// Every playable player with a known address (zone bridges excluded).
    pub players: Vec<Player>,
    pub groups: Vec<ZoneGroup>,
    /// Logical rooms (a stereo pair or home-theater set is one room).
    pub rooms: Vec<Room>,
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

    /// The player with id `id`, if it is known and addressable.
    #[must_use]
    pub fn player(&self, id: &PlayerId) -> Option<&Player> {
        self.players.iter().find(|p| p.id == *id)
    }

    /// The household's software generation, as its players report it.
    #[must_use]
    pub fn generation(&self) -> Option<Generation> {
        self.players.first().map(|p| p.generation)
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
