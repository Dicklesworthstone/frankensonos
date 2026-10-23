//! Durable post-edit identity and a fresh ownership observation for staging.

use super::unavailable;
use crate::scenes::queue_backup::{self, Owner};
use crate::snapshot::MemberLevel;
use crate::{CoreError, HouseholdState, control};
use fsonos_proto::content::{self, BrowseFlag, QUEUE};
use fsonos_proto::control as soap;
use fsonos_proto::didl::spotify_uri_from_renderer_uri;
use fsonos_proto::topology::{ZoneMember, get_zone_group_state};
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::{HouseholdId, PlayMode, PlayerId, TransportState};
use serde::{Deserialize, Serialize};
use std::cell::Cell;
use std::net::IpAddr;
use std::time::Instant;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Physical {
    player: PlayerId,
    host: IpAddr,
    boot: Option<u32>,
    primary: PlayerId,
    channels: Vec<(PlayerId, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Binding {
    pub(super) coordinator: PlayerId,
    household: Option<HouseholdId>,
    players: Vec<Physical>,
}

impl Binding {
    fn read<T: Transport + ?Sized>(
        t: &T,
        households: &[HouseholdState],
        coordinator: &PlayerId,
    ) -> Result<Self, CoreError> {
        let household = households
            .iter()
            .find(|house| house.player(coordinator).is_some())
            .ok_or_else(|| unavailable("the queue coordinator is missing"))?;
        let host = control::locate(households, coordinator)?.ip;
        let topology = get_zone_group_state(t, host)?;
        let mut groups = topology
            .groups
            .iter()
            .filter(|g| g.coordinator == *coordinator);
        let group = groups
            .next()
            .ok_or_else(|| unavailable("the player no longer leads its group"))?;
        if groups.next().is_some() {
            return Err(unavailable("the coordinator's group is ambiguous"));
        }
        let mut live = HouseholdState::default();
        live.apply_topology(&topology);
        let rooms: Vec<_> = live
            .rooms
            .iter()
            .filter(|r| r.coordinator == *coordinator)
            .collect();
        if rooms.is_empty() || rooms.iter().any(|r| !r.missing.is_empty()) {
            return Err(unavailable(
                "the queue's physical room membership is incomplete",
            ));
        }
        let present: Vec<_> = group
            .members
            .iter()
            .flat_map(|m| std::iter::once(m).chain(&m.satellites))
            .filter(|m| !m.is_zone_bridge)
            .collect();
        let mut players = Vec::with_capacity(present.len());
        for member in &present {
            let copies = topology
                .groups
                .iter()
                .flat_map(|g| &g.members)
                .flat_map(|m| std::iter::once(m).chain(&m.satellites))
                .filter(|m| m.uuid == member.uuid)
                .count();
            if copies != 1
                || member
                    .bonded_players()
                    .any(|id| !present.iter().any(|m| &m.uuid == id))
            {
                return Err(unavailable(
                    "a physical player is missing or listed more than once",
                ));
            }
            let room = rooms
                .iter()
                .find(|r| r.players.contains(&member.uuid))
                .ok_or_else(|| unavailable("a physical player has no unambiguous room"))?;
            players.push(Physical {
                player: member.uuid.clone(),
                host: member
                    .ip()
                    .ok_or_else(|| unavailable("a physical player has no address"))?,
                boot: member.boot_seq,
                primary: room.primary.clone(),
                channels: channels(member),
            });
        }
        players.sort_by(|a, b| a.player.0.cmp(&b.player.0));
        if !players
            .iter()
            .any(|p| p.player == *coordinator && p.host == host)
        {
            return Err(unavailable(
                "the original coordinator is absent at its address",
            ));
        }
        Ok(Self {
            coordinator: coordinator.clone(),
            household: household.id.clone(),
            players,
        })
    }

    pub(super) fn host(&self, player: &PlayerId) -> Result<IpAddr, CoreError> {
        self.players
            .iter()
            .find(|p| p.player == *player)
            .map(|p| p.host)
            .ok_or_else(|| unavailable("a bound player is missing"))
    }

    pub(super) fn boot(&self) -> Option<u32> {
        self.players
            .iter()
            .find(|p| p.player == self.coordinator)
            .and_then(|p| p.boot)
    }

    pub(super) fn primaries(&self) -> Vec<PlayerId> {
        let mut players: Vec<_> = self.players.iter().map(|p| p.primary.clone()).collect();
        players.sort_by(|a, b| a.0.cmp(&b.0));
        players.dedup();
        players
    }

    pub(super) fn confirm<T: Transport + ?Sized>(
        &self,
        t: &T,
        households: &[HouseholdState],
    ) -> Result<(), CoreError> {
        if Self::read(t, households, &self.coordinator)? != *self {
            return Err(unavailable(
                "the physical group, player address, household or boot changed",
            ));
        }
        Ok(())
    }

    fn levels<T: Transport + ?Sized>(&self, t: &T) -> Result<Vec<MemberLevel>, CoreError> {
        self.primaries()
            .iter()
            .map(|player| {
                let host = self.host(player)?;
                Ok(MemberLevel {
                    player: player.clone(),
                    volume: soap::get_volume(t, host)?,
                    mute: soap::get_mute(t, host)?,
                })
            })
            .collect()
    }
}

fn channels(member: &ZoneMember) -> Vec<(PlayerId, String)> {
    let mut channels: Vec<_> = member
        .channel_map
        .iter()
        .map(|c| (c.player.clone(), format!("stereo:{}", c.channels)))
        .chain(
            member
                .ht_sat_chan_map
                .iter()
                .map(|c| (c.player.clone(), format!("theater:{}", c.channels))),
        )
        .collect();
    channels.sort_by(|a, b| (&a.0.0, &a.1).cmp(&(&b.0.0, &b.1)));
    channels
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Playback {
    pub(super) source: String,
    pub(super) metadata: String,
    pub(super) track: u32,
    pub(super) uri: String,
    pub(super) position: Option<u32>,
    pub(super) duration: Option<u32>,
    pub(super) state: TransportState,
    pub(super) mode: Option<PlayMode>,
}

impl Playback {
    fn from_owner(owner: &Owner) -> Self {
        let p = &owner.playback;
        Self {
            source: p.media.uri.clone(),
            metadata: p.media.uri_metadata.clone(),
            track: p.position.track,
            uri: p.position.uri.clone(),
            position: p.position.position_secs,
            duration: p.position.duration_secs,
            state: p.state,
            mode: owner.mode,
        }
    }

    pub(super) fn time_matches(&self, other: &Self, elapsed: u32) -> bool {
        let forward = if self.state == TransportState::Playing {
            elapsed.saturating_add(1)
        } else {
            0
        };
        match (self.position, other.position) {
            (Some(before), Some(after)) => {
                after >= before && after <= before.saturating_add(forward)
            }
            (None, None) => true,
            _ => false,
        }
    }

    pub(super) fn agrees(&self, other: &Self, elapsed: u32) -> bool {
        self.source == other.source
            && self.metadata == other.metadata
            && self.track == other.track
            && same_track(&self.uri, &other.uri)
            && self.duration == other.duration
            && self.state == other.state
            && self.mode == other.mode
            && self.time_matches(other, elapsed)
    }
}

pub(super) fn same_track(a: &str, b: &str) -> bool {
    a == b
        || spotify_uri_from_renderer_uri(a)
            .is_some_and(|uri| spotify_uri_from_renderer_uri(b).as_deref() == Some(uri.as_str()))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Proof {
    pub(super) binding: Binding,
    pub(super) revision: u32,
    pub(super) uris: Vec<String>,
    pub(super) playback: Playback,
    pub(super) levels: Vec<MemberLevel>,
    pub(super) at: i64,
}

impl Proof {
    pub(super) fn read<T: Transport + ?Sized>(
        t: &T,
        households: &[HouseholdState],
        coordinator: &PlayerId,
        mode_known: bool,
        at: i64,
    ) -> Result<(Self, Owner), CoreError> {
        let started = Instant::now();
        let binding = Binding::read(t, households, coordinator)?;
        let host = binding.host(coordinator)?;
        let first = Owner::read(t, host, coordinator, mode_known)?;
        let queue = queue_backup::bounded_snapshot(t, host)?;
        let levels = binding.levels(t)?;
        first.confirm(t, host, coordinator)?;
        let owner = Owner::read(t, host, coordinator, mode_known)?;
        let observed_at = super::later(at, started);
        first.confirm(t, host, coordinator)?;
        owner.confirm(t, host, coordinator)?;
        binding.confirm(t, households)?;
        let head = content::browse(t, host, QUEUE, BrowseFlag::DirectChildren, 0, 1)?;
        if queue.update_id == 0
            || head.update_id != queue.update_id
            || usize::try_from(head.total_matches).ok() != Some(queue.objects.len())
        {
            return Err(unavailable(
                "the queue revision changed or is a native wildcard",
            ));
        }
        let uris = queue
            .objects
            .iter()
            .map(|item| {
                item.res
                    .as_ref()
                    .map(|res| res.uri.clone())
                    .filter(|uri| !uri.is_empty() && uri != "NOT_IMPLEMENTED")
            })
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| unavailable("a queue entry has no renderer identity"))?;
        let playback = Playback::from_owner(&owner);
        if playback.source != format!("x-rincon-queue:{}#0", coordinator.0)
            || !matches!(
                playback.state,
                TransportState::Playing | TransportState::Paused | TransportState::Stopped
            )
            || (uris.is_empty() && (playback.track != 0 || !playback.uri.is_empty()))
            || (!uris.is_empty()
                && playback
                    .track
                    .checked_sub(1)
                    .and_then(|n| usize::try_from(n).ok())
                    .and_then(|n| uris.get(n))
                    .is_none_or(|uri| !same_track(uri, &playback.uri)))
        {
            return Err(unavailable(
                "the player is not stably selecting its own queue",
            ));
        }
        Ok((
            Self {
                binding,
                revision: queue.update_id,
                uris,
                playback,
                levels,
                at: observed_at,
            },
            owner,
        ))
    }

    pub(super) fn confirm<T: Transport + ?Sized>(
        &self,
        t: &T,
        households: &[HouseholdState],
        at: i64,
    ) -> Result<Owner, CoreError> {
        let (now, owner) = Self::read(
            t,
            households,
            &self.binding.coordinator,
            self.playback.mode.is_some(),
            at,
        )?;
        let elapsed = u32::try_from(now.at.saturating_sub(self.at)).unwrap_or(0);
        if now.at < self.at
            || self.binding != now.binding
            || self.revision != now.revision
            || self.uris != now.uris
            || self.levels != now.levels
            || !self.playback.agrees(&now.playback, elapsed)
        {
            return Err(unavailable(
                "the logged queue, selected occurrence, playback, levels or physical group changed; force cannot bypass queue ownership",
            ));
        }
        Ok(owner)
    }

    pub(super) fn confirm_levels<T: Transport + ?Sized>(&self, t: &T) -> Result<(), CoreError> {
        if self.binding.levels(t)? != self.levels {
            return Err(unavailable("the room levels changed during queue undo"));
        }
        Ok(())
    }
}

/// Staging already fences queue revisions and playback on every write. Keep
/// its physical player and capped level proof alive through the final Play.
pub(super) struct Fenced<'a, T: ?Sized> {
    pub(super) t: &'a T,
    pub(super) households: &'a [HouseholdState],
    pub(super) proof: &'a Proof,
    pub(super) poisoned: Cell<bool>,
}

impl<T: Transport + ?Sized> Transport for Fenced<'_, T> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action
            .trim_matches('"')
            .rsplit('#')
            .next()
            .unwrap_or(action);
        if !name.starts_with("Get") && name != "Browse" {
            if self.poisoned.get() {
                return Err(ProtoError::Malformed(
                    "an earlier queue undo ownership fence failed; no further writes are allowed"
                        .into(),
                ));
            }
            if let Err(error) = self
                .proof
                .binding
                .confirm(self.t, self.households)
                .and_then(|()| self.proof.confirm_levels(self.t))
            {
                self.poisoned.set(true);
                return Err(ProtoError::Malformed(error.to_string()));
            }
        }
        self.t.soap_post(host, path, action, body)
    }
}
