//! Resolve a command's smart target before acting, using the core resolver.
//! Room-level actions address `rooms`; transport affects every member of
//! `groups`. This is an observation, not a reservation or authorization to act.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::policy::Client;
use fsonos_core::rooms::{Aliases, Matched, ResolveContext, resolve_targets};
use fsonos_core::{HouseholdState, Room};
use fsonos_types::TransportState;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::Surface;
use crate::failure::{ErrorCode, Failure};
use crate::plan::Rooms;
use crate::request::ZoneRequest;
use crate::zones::zone_for_target;

/// A room identity in a resolved target or one of its affected groups.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TargetRoomDto {
    pub room: String,
    pub household: String,
    pub player: String,
    pub coordinator: String,
}

/// All members of a group touched by a transport command, not just the
/// alias's named rooms. No playback state is fetched to build this view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TargetGroupDto {
    pub household: String,
    pub coordinator: String,
    pub coordinator_room: String,
    pub members: Vec<TargetRoomDto>,
}

/// `GET /rooms/resolve?target=...`: the current meaning of a room, alias,
/// `here`, or `everywhere` (optionally qualified by household).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ResolvedRoomsDto {
    pub target: String,
    /// Opaque observation of these identities, complete physical groups and
    /// addresses. Reviewed DJ controls require it; it grants no permission.
    pub revision: String,
    /// `room`, `alias`, `here`, `everywhere`, or `prefix`.
    pub matched: String,
    /// Currently addressable rooms selected by the core resolver.
    pub rooms: Vec<TargetRoomDto>,
    /// Complete groups containing the selected rooms, with no duplicates.
    pub groups: Vec<TargetGroupDto>,
}

fn room_view(room: &Room, household: &str) -> TargetRoomDto {
    TargetRoomDto {
        room: room.name.clone(),
        household: household.to_string(),
        player: room.primary.0.clone(),
        coordinator: room.coordinator.0.clone(),
    }
}

fn resolved(
    households: &[HouseholdState],
    aliases: Option<&Aliases>,
    client: &Client,
    query: &str,
) -> Result<ResolvedRoomsDto, Failure> {
    resolve_view(
        &Rooms {
            households,
            aliases,
            client: Some(client.key()),
        },
        query,
    )
}

/// Build the same observation inside a command's admitted planning snapshot.
pub(crate) fn resolve_view(rooms: &Rooms<'_>, query: &str) -> Result<ResolvedRoomsDto, Failure> {
    let households = rooms.households;
    let resolution = resolve_targets(
        households,
        query,
        ResolveContext {
            aliases: rooms.aliases,
            client: rooms.client,
        },
    )?;
    let matched = match resolution.matched {
        Matched::Room => "room",
        Matched::Alias => "alias",
        Matched::Here => "here",
        Matched::Everywhere => "everywhere",
        Matched::Prefix => "prefix",
    };
    let mut rooms = Vec::new();
    let mut groups: Vec<TargetGroupDto> = Vec::new();
    let mut physical = Vec::new();
    for target in &resolution.targets {
        let zone = zone_for_target(households, target, |_| TransportState::Unknown);
        rooms.push(room_view(target.room, &zone.household));
        if groups.iter().any(|group| {
            group.household == zone.household && group.coordinator == target.coordinator.id.0
        }) {
            continue;
        }
        let mut members: Vec<_> = target
            .household
            .rooms
            .iter()
            .filter(|room| room.coordinator == target.coordinator.id)
            .map(|room| room_view(room, &zone.household))
            .collect();
        members.sort_by(|a, b| (&a.room, &a.player).cmp(&(&b.room, &b.player)));
        // Room primaries alone miss stereo partners, satellites and address
        // changes. Include every physical member even when it has no address.
        let mut players: Vec<_> = target
            .household
            .groups
            .iter()
            .filter(|group| group.coordinator == target.coordinator.id)
            .flat_map(|group| &group.members)
            .map(|id| {
                (
                    id.0.clone(),
                    target.household.player(id).map(|player| player.ip),
                )
            })
            .collect();
        players.sort();
        players.dedup();
        physical.push((
            zone.household.clone(),
            target.household.id.as_ref().map(|id| id.0.clone()),
            target.coordinator.id.0.clone(),
            target.coordinator.ip,
            players,
        ));
        groups.push(TargetGroupDto {
            household: zone.household,
            coordinator: target.coordinator.id.0.clone(),
            coordinator_room: zone.coordinator_room,
            members,
        });
    }
    rooms.sort_by(|a, b| {
        (&a.household, &a.room, &a.player).cmp(&(&b.household, &b.room, &b.player))
    });
    groups.sort_by(|a, b| (&a.household, &a.coordinator).cmp(&(&b.household, &b.coordinator)));
    physical.sort();
    let observation = serde_json::to_vec(&(1_u8, query, matched, &rooms, &groups, physical))
        .map_err(|error| Failure::new(ErrorCode::Internal, error.to_string()))?;
    let revision = format!("{:x}", Sha256::digest(observation));
    Ok(ResolvedRoomsDto {
        target: query.to_string(),
        revision,
        matched: matched.to_string(),
        rooms,
        groups,
    })
}

impl Surface {
    /// Inspect the exact core resolution, including explicitly qualified
    /// aliases that select rooms in several households. Ambiguous entries
    /// fail as they do for commands; `/rooms`' alias labels are only hints.
    pub fn resolve_rooms(
        &self,
        client: &Client,
        target: &str,
    ) -> Result<ResolvedRoomsDto, Failure> {
        // Reveal neither rooms nor their group membership without the same
        // read permissions their existing listing endpoints require.
        let guard = self.guard(client);
        guard.authorize("resolve_rooms", true)?;
        guard.authorize("list_rooms", true)?;
        guard.authorize("list_zones", true)?;
        let request = ZoneRequest {
            zone: target.to_string(),
        };
        let target = request.zone()?;
        let households = self.households()?;
        let aliases = self.aliases();
        resolved(&households, aliases.as_ref(), client, target).map_err(|error| self.explain(error))
    }
}

#[cfg(test)]
mod tests;
