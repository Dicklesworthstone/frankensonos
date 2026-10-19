//! Resolve a command's smart target before acting, using the core resolver.
//! Room-level actions address `rooms`; transport affects every member of
//! `groups`. This is an observation, not a reservation or authorization to act.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::policy::Client;
use fsonos_core::rooms::{Aliases, Matched, ResolveContext, resolve_targets};
use fsonos_core::{HouseholdState, Room};
use fsonos_types::TransportState;
use serde::{Deserialize, Serialize};

use super::Surface;
use crate::failure::Failure;
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
    let resolution = resolve_targets(
        households,
        query,
        ResolveContext {
            aliases,
            client: Some(client.key()),
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
    Ok(ResolvedRoomsDto {
        target: query.to_string(),
        matched: matched.to_string(),
        rooms,
        groups,
    })
}

impl Surface {
    /// Inspect the exact core resolution, including aliases whose unqualified
    /// entries select rooms in several households. `/rooms`' alias labels are
    /// discovery hints, not enough information to reproduce this resolution.
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
