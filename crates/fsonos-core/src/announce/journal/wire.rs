//! Private serde adapters keep persisted announcement evidence local to this
//! feature rather than making the live household model a public wire schema.

use crate::{HouseholdState, Room};
use fsonos_types::{HouseholdId, Player, PlayerId, ZoneGroup};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Serialize, Deserialize)]
struct SavedRoom {
    name: String,
    primary: PlayerId,
    players: Vec<PlayerId>,
    missing: Vec<PlayerId>,
    coordinator: PlayerId,
}

impl From<&Room> for SavedRoom {
    fn from(room: &Room) -> Self {
        Self {
            name: room.name.clone(),
            primary: room.primary.clone(),
            players: room.players.clone(),
            missing: room.missing.clone(),
            coordinator: room.coordinator.clone(),
        }
    }
}

impl From<SavedRoom> for Room {
    fn from(room: SavedRoom) -> Self {
        Self {
            name: room.name,
            primary: room.primary,
            players: room.players,
            missing: room.missing,
            coordinator: room.coordinator,
        }
    }
}

pub(in crate::announce) mod room_vector {
    use super::{Deserialize, Deserializer, Room, SavedRoom, Serialize, Serializer};

    pub(in crate::announce) fn serialize<S: Serializer>(
        rooms: &[Room],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        rooms
            .iter()
            .map(SavedRoom::from)
            .collect::<Vec<_>>()
            .serialize(serializer)
    }

    pub(in crate::announce) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<Room>, D::Error> {
        Ok(Vec::<SavedRoom>::deserialize(deserializer)?
            .into_iter()
            .map(Room::from)
            .collect())
    }
}

#[derive(Serialize, Deserialize)]
struct SavedHousehold {
    id: Option<HouseholdId>,
    players: Vec<Player>,
    groups: Vec<ZoneGroup>,
    rooms: Vec<SavedRoom>,
}

pub(super) mod household {
    use super::{
        Deserialize, Deserializer, HouseholdState, Room, SavedHousehold, SavedRoom, Serialize,
        Serializer,
    };

    pub(in crate::announce) fn serialize<S: Serializer>(
        household: &HouseholdState,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        SavedHousehold {
            id: household.id.clone(),
            players: household.players.clone(),
            groups: household.groups.clone(),
            rooms: household.rooms.iter().map(SavedRoom::from).collect(),
        }
        .serialize(serializer)
    }

    pub(in crate::announce) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<HouseholdState, D::Error> {
        let household = SavedHousehold::deserialize(deserializer)?;
        Ok(HouseholdState {
            id: household.id,
            players: household.players,
            groups: household.groups,
            rooms: household.rooms.into_iter().map(Room::from).collect(),
        })
    }
}
