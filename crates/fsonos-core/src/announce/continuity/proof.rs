//! Identity and playback evidence for native announcement handoffs.

use crate::{CoreError, HouseholdState, Room};
use fsonos_proto::content::{self, BrowseFlag};
use fsonos_proto::control::{self as soap, MediaInfo, PositionInfo};
use fsonos_proto::didl::spotify_uri_from_renderer_uri;
use fsonos_proto::play_mode::get_play_mode;
use fsonos_proto::topology::{ZoneGroupState, get_zone_group_state};
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::{PlayMode, PlayerId, TransportState};
use std::collections::HashSet;
use std::net::IpAddr;
use std::time::{Duration, Instant};

const GROUP_WAIT: Duration = Duration::from_secs(3);
const GROUP_POLL: Duration = Duration::from_millis(100);

pub(super) fn changed(why: &str) -> CoreError {
    ProtoError::Malformed(format!(
        "announcement continuity could not be confirmed: {why}; the live group was not replayed from an old snapshot"
    ))
    .into()
}

fn same_ids(a: &[PlayerId], b: &[PlayerId]) -> bool {
    a.len() == b.len() && a.iter().all(|id| b.contains(id))
}

#[derive(Clone)]
struct Identity {
    player: PlayerId,
    host: IpAddr,
    boot: Option<u32>,
}

#[derive(Clone)]
pub(super) struct Binding {
    identities: Vec<Identity>,
    rooms: Vec<Room>,
}

/// Every affected physical player, including bonded halves, belongs to
/// exactly one expected group. Group IDs may change while regrouping.
#[derive(Clone)]
pub(super) struct Layout(pub(super) Vec<(PlayerId, Vec<PlayerId>)>);

impl Layout {
    pub(super) fn depart(&mut self, room: &Room, heir: Option<&PlayerId>) {
        for (coordinator, members) in &mut self.0 {
            if members.contains(&room.primary) {
                members.retain(|p| !room.players.contains(p));
                if let Some(heir) = heir {
                    coordinator.clone_from(heir);
                }
                break;
            }
        }
        self.0.retain(|(_, members)| !members.is_empty());
        self.0.push((room.primary.clone(), room.players.clone()));
    }

    pub(super) fn join(&mut self, room: &Room, into: &PlayerId) {
        self.0
            .retain(|(coordinator, _)| coordinator != &room.primary);
        if let Some((_, members)) = self
            .0
            .iter_mut()
            .find(|(coordinator, _)| coordinator == into)
        {
            members.extend(room.players.iter().cloned());
        }
    }

    pub(super) fn delegate(&mut self, from: &PlayerId, to: &PlayerId) {
        if let Some((coordinator, _)) = self
            .0
            .iter_mut()
            .find(|(coordinator, _)| coordinator == from)
        {
            coordinator.clone_from(to);
        }
    }
}

impl Binding {
    pub(super) fn capture<T: Transport + ?Sized>(
        t: &T,
        household: &HouseholdState,
        coordinator: &PlayerId,
    ) -> Result<(Self, Layout), CoreError> {
        let rooms: Vec<_> = household
            .rooms
            .iter()
            .filter(|room| room.coordinator == *coordinator)
            .cloned()
            .collect();
        let members: Vec<_> = rooms.iter().flat_map(|room| room.players.clone()).collect();
        let host = household
            .player(coordinator)
            .ok_or_else(|| changed("the original coordinator disappeared"))?
            .ip;
        let topology = get_zone_group_state(t, host)?;
        let mut identities = Vec::new();
        for player in &members {
            let known = household
                .player(player)
                .ok_or_else(|| changed("a bonded player's address is unknown"))?;
            let boot = topology
                .boot_seqs()
                .find_map(|(id, seq)| (id == player).then_some(seq));
            identities.push(Identity {
                player: player.clone(),
                host: known.ip,
                boot,
            });
        }
        let binding = Self { identities, rooms };
        let layout = Layout(vec![(coordinator.clone(), members)]);
        binding.verify(&topology, &layout)?;
        Ok((binding, layout))
    }

    pub(super) fn room(&self, player: &PlayerId) -> Result<&Room, CoreError> {
        self.rooms
            .iter()
            .find(|room| room.primary == *player)
            .ok_or_else(|| changed("a room's primary changed"))
    }

    pub(super) fn host(&self, player: &PlayerId) -> Result<IpAddr, CoreError> {
        self.identities
            .iter()
            .find(|id| id.player == *player)
            .map(|id| id.host)
            .ok_or_else(|| changed("a player's identity is missing"))
    }

    pub(super) fn whole(&self, coordinator: &PlayerId) -> Layout {
        Layout(vec![(
            coordinator.clone(),
            self.identities.iter().map(|id| id.player.clone()).collect(),
        )])
    }

    fn identities_match(&self, topology: &ZoneGroupState) -> Result<(), CoreError> {
        for identity in &self.identities {
            let mut found = topology
                .groups
                .iter()
                .flat_map(|g| &g.members)
                .flat_map(|m| std::iter::once(m).chain(&m.satellites))
                .filter(|m| m.uuid == identity.player);
            let member = found
                .next()
                .ok_or_else(|| changed("an original player is missing"))?;
            if found.next().is_some()
                || member.ip() != Some(identity.host)
                || member.boot_seq != identity.boot
            {
                return Err(changed(
                    "a player rebooted, moved address, or has ambiguous identity",
                ));
            }
        }
        Ok(())
    }

    fn verify(&self, topology: &ZoneGroupState, layout: &Layout) -> Result<(), CoreError> {
        self.identities_match(topology)?;
        for (coordinator, expected) in &layout.0 {
            let mut groups = topology
                .groups
                .iter()
                .filter(|g| g.coordinator == *coordinator);
            let group = groups
                .next()
                .ok_or_else(|| changed("an expected coordinator changed"))?;
            let actual: Vec<_> = group
                .members
                .iter()
                .flat_map(|m| std::iter::once(m).chain(&m.satellites))
                .map(|m| m.uuid.clone())
                .collect();
            if groups.next().is_some()
                || !same_ids(expected, &actual)
                || actual.iter().collect::<HashSet<_>>().len() != actual.len()
                || !actual.contains(coordinator)
            {
                return Err(changed("group membership changed"));
            }
        }
        let mut now = HouseholdState::default();
        now.apply_topology(topology);
        for room in &self.rooms {
            if !now.rooms.iter().any(|r| {
                r.primary == room.primary
                    && same_ids(&r.players, &room.players)
                    && same_ids(&r.missing, &room.missing)
            }) {
                return Err(changed("a bonded room changed"));
            }
        }
        Ok(())
    }

    pub(super) fn check<T: Transport + ?Sized>(
        &self,
        t: &T,
        layout: &Layout,
        witness: &PlayerId,
    ) -> Result<HouseholdState, CoreError> {
        self.observe(t, layout, witness, false)
    }

    /// After our own write a coordinator can serve its pre-handoff topology.
    /// One independent witness may prove the intended transition. Pre-write
    /// checks never discard a mismatching layout in favor of an older view.
    pub(super) fn after<T: Transport + ?Sized>(
        &self,
        t: &T,
        layout: &Layout,
        witness: &PlayerId,
    ) -> Result<HouseholdState, CoreError> {
        self.observe(t, layout, witness, true)
    }

    fn observe<T: Transport + ?Sized>(
        &self,
        t: &T,
        layout: &Layout,
        witness: &PlayerId,
        transition: bool,
    ) -> Result<HouseholdState, CoreError> {
        let host = self.host(witness)?;
        let own = self
            .rooms
            .iter()
            .find(|room| room.players.contains(witness));
        let other = self
            .identities
            .iter()
            .find(|id| id.host != host && own.is_none_or(|room| !room.players.contains(&id.player)))
            .or_else(|| self.identities.iter().find(|id| id.host != host));
        let hosts = [Some(host), other.map(|id| id.host)];
        let started = Instant::now();
        let mut read_any = false;
        let mut last = changed("no original player answered");
        loop {
            for host in hosts.into_iter().flatten() {
                if transition && read_any && started.elapsed() >= GROUP_WAIT {
                    return Err(last);
                }
                read_any = true;
                let topology = match get_zone_group_state(t, host) {
                    Ok(topology) => topology,
                    Err(error) => {
                        last = error.into();
                        continue;
                    }
                };
                // Reboots and identity conflicts are not stale grouping evidence.
                self.identities_match(&topology)?;
                match self.verify(&topology, layout) {
                    Ok(()) => {
                        let mut now = HouseholdState::default();
                        now.apply_topology(&topology);
                        return Ok(now);
                    }
                    Err(error) if transition => last = error,
                    Err(error) => return Err(error),
                }
            }
            let remaining = GROUP_WAIT.saturating_sub(started.elapsed());
            if !transition || remaining.is_zero() {
                return Err(last);
            }
            std::thread::sleep(GROUP_POLL.min(remaining));
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
struct Queue {
    revision: u32,
    uris: Vec<String>,
}

/// A stable current selection. It is captured again when returning the
/// rooms, so progress, mode changes and queue edits during the clip survive.
#[derive(Clone)]
pub(super) struct Playback {
    media: MediaInfo,
    position: PositionInfo,
    state: TransportState,
    mode: Option<PlayMode>,
    queue: Option<Queue>,
    observed: Instant,
}

fn same_track(a: &str, b: &str) -> bool {
    a == b
        || spotify_uri_from_renderer_uri(a)
            .is_some_and(|uri| spotify_uri_from_renderer_uri(b).as_deref() == Some(uri.as_str()))
}

impl Playback {
    pub(super) fn capture<T: Transport + ?Sized>(
        t: &T,
        host: IpAddr,
        coordinator: &PlayerId,
    ) -> Result<Self, CoreError> {
        let observed = Instant::now();
        let media = soap::get_media_info(t, host)?;
        if media.uri.starts_with("x-rincon:") {
            return Err(changed("the expected coordinator became a member"));
        }
        let position = soap::get_position_info(t, host)?;
        let state = soap::get_transport_info(t, host)?.state;
        if !matches!(
            state,
            TransportState::Playing | TransportState::Paused | TransportState::Stopped
        ) {
            return Err(changed("playback is still transitioning"));
        }
        let queue = if media.uri == format!("x-rincon-queue:{}#0", coordinator.0) {
            let snapshot = content::browse_snapshot(t, host, content::QUEUE)?;
            let uris = snapshot
                .objects
                .iter()
                .map(|item| {
                    item.res
                        .as_ref()
                        .map(|res| res.uri.clone())
                        .ok_or_else(|| changed("a queue entry has no resource"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            if position.track > 0
                && !usize::try_from(position.track - 1)
                    .ok()
                    .and_then(|index| uris.get(index))
                    .is_some_and(|uri| same_track(uri, &position.uri))
            {
                return Err(changed("the selected queue occurrence changed"));
            }
            Some(Queue {
                revision: snapshot.update_id,
                uris,
            })
        } else {
            if media.uri.starts_with("x-rincon-queue:") {
                return Err(changed("the player points at another coordinator's queue"));
            }
            None
        };
        let mode = queue.as_ref().and_then(|_| get_play_mode(t, host).ok());
        let captured = Self {
            media,
            position,
            state,
            mode,
            queue,
            observed,
        };
        captured.confirm(t, host, coordinator)?;
        Ok(captured)
    }

    pub(super) fn is_stopped_clip(&self, clip: &str) -> bool {
        self.media.uri == clip && self.state == TransportState::Stopped
    }

    fn agrees(
        &self,
        media: &MediaInfo,
        position: &PositionInfo,
        state: TransportState,
        coordinator: &PlayerId,
    ) -> bool {
        let expected_uri = if self.queue.is_some() {
            format!("x-rincon-queue:{}#0", coordinator.0)
        } else {
            self.media.uri.clone()
        };
        let advance = if self.state == TransportState::Playing {
            u32::try_from(self.observed.elapsed().as_secs())
                .unwrap_or(u32::MAX)
                .saturating_add(1)
        } else {
            0
        };
        let position_agrees = match (self.position.position_secs, position.position_secs) {
            (Some(before), Some(after)) => {
                after >= before && after <= before.saturating_add(advance)
            }
            (None, None) => true,
            _ => false,
        };
        media.uri == expected_uri
            && media.uri_metadata == self.media.uri_metadata
            && media.tracks == self.media.tracks
            && state == self.state
            && position.track == self.position.track
            && same_track(&position.uri, &self.position.uri)
            && position.duration_secs == self.position.duration_secs
            && position_agrees
    }

    pub(super) fn confirm<T: Transport + ?Sized>(
        &self,
        t: &T,
        host: IpAddr,
        coordinator: &PlayerId,
    ) -> Result<(), CoreError> {
        let position = soap::get_position_info(t, host)?;
        if let Some(queue) = &self.queue {
            let page = content::browse(t, host, content::QUEUE, BrowseFlag::DirectChildren, 0, 1)?;
            if page.update_id != queue.revision
                || usize::try_from(page.total_matches).ok() != Some(queue.uris.len())
            {
                return Err(changed("the live queue changed during a handoff"));
            }
        }
        if self
            .mode
            .is_some_and(|mode| !get_play_mode(t, host).is_ok_and(|actual| actual == mode))
        {
            return Err(changed("the live play mode changed during a handoff"));
        }
        let state = soap::get_transport_info(t, host)?.state;
        let media = soap::get_media_info(t, host)?;
        if !self.agrees(&media, &position, state, coordinator) {
            return Err(changed(
                "live playback was replaced, sought, paused, or restarted",
            ));
        }
        Ok(())
    }

    pub(super) fn transferred<T: Transport + ?Sized>(
        &self,
        t: &T,
        host: IpAddr,
        coordinator: &PlayerId,
    ) -> Result<(), CoreError> {
        self.confirm(t, host, coordinator)?;
        if let Some(queue) = &self.queue {
            let actual = content::browse_snapshot(t, host, content::QUEUE)?;
            if actual.update_id != queue.revision
                || actual.objects.len() != queue.uris.len()
                || actual.objects.iter().zip(&queue.uris).any(|(item, uri)| {
                    !item
                        .res
                        .as_ref()
                        .is_some_and(|res| same_track(&res.uri, uri))
                })
            {
                return Err(changed("the complete live queue was not transferred"));
            }
        }
        self.confirm(t, host, coordinator)
    }
}
