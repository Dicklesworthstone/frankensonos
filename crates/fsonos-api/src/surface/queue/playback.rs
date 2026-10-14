//! Start an existing queue entry without adding, replacing or clearing tracks.
//! The selected slot is checked again after snapshot/cap reads. The final
//! device writes use core's staged queue start and never replay the whole start.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::actions;
use fsonos_core::policy::Client;
use fsonos_core::{CoreError, HouseholdState, control};
use fsonos_proto::content::{BrowseSnapshot, QUEUE, browse_range_snapshot};
use fsonos_proto::topology::{ZoneGroupInfo, get_zone_group_state};
use fsonos_types::PlayerId;
use serde::{Deserialize, Serialize};
use std::net::IpAddr;

use super::super::{Surface, log_entry, resolve, room_view};
use crate::OutcomeDto;
use crate::failure::{ErrorCode, Failure, NoteCode};
use crate::guard::{Guard, Note};

/// Select an existing queue entry and start it (`POST /queue/play`).
pub const PLAY: &str = "play_queue";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QueuePlayRequest {
    pub zone: String,
    /// The existing queue position, starting at one.
    pub position: u32,
    /// Optional renderer URI from `get_queue`. A different item at this
    /// position is rejected instead of playing a stale selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_uri: Option<String>,
}

impl QueuePlayRequest {
    fn validate(&self) -> Result<(), Failure> {
        if self.zone.trim().is_empty() || self.position == 0 {
            return Err(Failure::invalid(
                "give a room and a queue position starting at one",
            ));
        }
        if self
            .expected_uri
            .as_deref()
            .is_some_and(|uri| uri.trim().is_empty())
        {
            return Err(Failure::invalid("expected_uri must be nonempty when supplied"));
        }
        Ok(())
    }

    fn check(&self, page: &BrowseSnapshot) -> Result<String, Failure> {
        let [item] = page.objects.as_slice() else {
            return Err(Failure::new(
                ErrorCode::NoMatch,
                format!("no track at queue position {}", self.position),
            )
            .with_hint("Read the room's queue and choose an existing position."));
        };
        let uri = item
            .res
            .as_ref()
            .map(|res| res.uri.as_str())
            .filter(|uri| !uri.trim().is_empty() && *uri != "NOT_IMPLEMENTED")
            .ok_or_else(|| Failure::invalid("the selected queue entry has no playable URI"))?;
        if self
            .expected_uri
            .as_deref()
            .is_some_and(|expected| expected != uri)
        {
            return Err(changed());
        }
        Ok(item.title.clone())
    }
}

fn changed() -> Failure {
    Failure::new(
        ErrorCode::NotReady,
        "the queue changed after the selection was made",
    )
    .with_hint("Read the queue again, then select its current position and URI.")
}

impl Surface {
    /// Activate `req`'s existing queue entry. Preserve queue contents and the
    /// owner's shuffle/repeat setting; playback and volume changes are undoable.
    pub fn queue_play(&self, client: &Client, req: &QueuePlayRequest) -> Result<OutcomeDto, Failure> {
        let guard = self.guard(client);
        if let Err(denied) = guard.authorize(PLAY, false) {
            self.record(
                client,
                PLAY.into(),
                format!("deny: {}", denied.detail),
                denied.detail.clone(),
                None,
            );
            return Err(denied);
        }
        req.validate()?;
        let (households, group) = self.queue_households(client, &req.zone)?;
        let coordinator = group.coordinator.clone();
        let player = control::locate(&households, &coordinator)?;
        let zone = player.room_name.clone();
        let host = player.ip;
        let read = || {
            browse_range_snapshot(&*self.transport, host, QUEUE, req.position - 1, 1)
                .map_err(|error| Failure::from(CoreError::from(error)))
        };
        let selected = read().inspect_err(|failure| self.notice(failure))?;
        let title = req.check(&selected)?;
        let (snaps, missed) = if self.log.is_some() {
            actions::capture_zones(
                &*self.transport,
                &households,
                std::slice::from_ref(&coordinator),
                self.now(),
            )
        } else {
            (Vec::new(), Vec::new())
        };
        let result = (|| {
            // Invalid or stale selections must not change even the volumes.
            if read()? != selected {
                return Err(changed());
            }
            self.queue_group_unchanged(host, &group)?;
            let notes = self.lower_queue_group(&households, &guard, &coordinator)?;
            // Snapshot and policy reads may take time. Never silently seek a
            // shifted slot, or detach a player that became a group member.
            self.queue_group_unchanged(host, &group)?;
            if read()? != selected {
                return Err(changed());
            }
            control::play_queue_from(&*self.transport, &households, &coordinator, req.position)?;
            Ok(OutcomeDto {
                done: format!(
                    "playing queue position {} in {zone}'s group: {title}",
                    req.position
                ),
                changed: true,
                volume: None,
                notes,
            })
        })();
        if let Err(failure) = &result {
            self.notice(failure);
        }
        let before = actions::before_state_with(&snaps, &[]);
        let (decision, text, before) = log_entry(&result, before, missed.len());
        self.record(
            client,
            format!("{PLAY}: {zone} position {}", req.position),
            decision,
            text,
            before,
        );
        result
    }

    /// Warm discovery locates a peer; fresh topology determines every room
    /// that may sound. Cached grouping alone cannot establish the volume caps.
    fn queue_households(
        &self,
        client: &Client,
        zone: &str,
    ) -> Result<(Vec<HouseholdState>, ZoneGroupInfo), Failure> {
        let mut households = self.households()?;
        let aliases = self.aliases();
        let target = resolve(room_view(&households, aliases.as_ref(), client), zone)
            .map_err(|failure| self.explain(failure))?;
        let member = target.player.id.clone();
        let topology = get_zone_group_state(&*self.transport, target.player.ip)
            .map_err(|error| Failure::from(CoreError::from(error)))?;
        let household = households
            .iter_mut()
            .find(|house| house.player(&member).is_some())
            .ok_or_else(|| Failure::new(ErrorCode::NotReady, "the room left its household"))?;
        household.apply_topology(&topology);
        let target = resolve(room_view(&households, aliases.as_ref(), client), &member.0)?;
        let group = topology
            .groups
            .into_iter()
            .find(|group| group.coordinator == target.coordinator.id)
            .ok_or_else(|| Failure::new(ErrorCode::NotCoordinator, "the room's group changed"))?;
        Ok((households, group))
    }

    fn queue_group_unchanged(&self, host: IpAddr, group: &ZoneGroupInfo) -> Result<(), Failure> {
        let fresh = get_zone_group_state(&*self.transport, host)
            .map_err(|error| Failure::from(CoreError::from(error)))?;
        if !fresh.groups.contains(group) {
            return Err(Failure::new(
                ErrorCode::NotCoordinator,
                "the room's group changed",
            )
            .with_hint("Read the room's current queue before selecting a position."));
        }
        Ok(())
    }

    /// Preflight every capped room before writing any levels. An unreadable
    /// grouped room might still sound, so failure cannot authorize loud playback.
    fn lower_queue_group(
        &self,
        households: &[HouseholdState],
        guard: &Guard<'_>,
        coordinator: &PlayerId,
    ) -> Result<Vec<Note>, Failure> {
        let now = guard.clock.now();
        let mut over = Vec::new();
        for room in households
            .iter()
            .flat_map(|house| &house.rooms)
            .filter(|room| room.coordinator == *coordinator)
        {
            if let Some((cap, why)) = guard.policy.cap_for(&room.name, guard.client, now) {
                let level = control::volume(&*self.transport, households, &room.primary)?;
                if level > cap {
                    over.push((room, level, cap, why));
                }
            }
        }
        let players: Vec<_> = over.iter().map(|(room, ..)| room.primary.clone()).collect();
        if !players.is_empty() {
            self.volumes_changing(households, &players);
        }
        let mut notes = Vec::new();
        for (room, level, cap, why) in over {
            control::set_volume(&*self.transport, households, &room.primary, cap)?;
            notes.push(Note {
                code: NoteCode::VolumeClamped,
                detail: format!("{} lowered from {level} to {cap} first: {why}", room.name),
            });
        }
        Ok(notes)
    }
}
