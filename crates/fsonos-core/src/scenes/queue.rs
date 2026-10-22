//! Recall an existing queue occurrence without changing any queue entries.
//! A reference is valid only on its original coordinator and queue revision.

use crate::snapshot::{self, Aspect, SnapshotSource, ZoneSnapshot};
use crate::{CoreError, HouseholdState, control};
use fsonos_proto::content::{self, BrowseFlag, QUEUE};
use fsonos_proto::control::{self as soap, MediaInfo, PositionInfo};
use fsonos_proto::didl::spotify_uri_from_renderer_uri;
use fsonos_proto::play_mode::get_play_mode;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::{HouseholdId, PlayMode, PlayerId, TransportState};
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::net::IpAddr;
use std::time::Instant;

/// A bookmark into the original queue, not a copy of its contents. Edited or
/// cleared queues must be saved again; duplicate tracks retain their slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueReference {
    pub coordinator: PlayerId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub household: Option<HouseholdId>,
    pub update_id: u32,
    pub track: u32,
    pub position_secs: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub play_mode: Option<PlayMode>,
    /// When reported, prevents a reboot from reusing an old queue revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_seq: Option<u32>,
}

impl QueueReference {
    pub(super) fn validate(&self) -> Result<(), String> {
        let identity = |value: &str| {
            !value.trim().is_empty() && value.len() <= 1024 && !value.chars().any(char::is_control)
        };
        if !identity(&self.coordinator.0)
            || self.household.as_ref().is_some_and(|id| !identity(&id.0))
            || self.track == 0
        {
            return Err(
                "a queue reference needs its original player and a selected occurrence".into(),
            );
        }
        Ok(())
    }

    fn uri(&self) -> String {
        format!("x-rincon-queue:{}#0", self.coordinator.0)
    }
}

fn invalid(detail: &str) -> ProtoError {
    ProtoError::Malformed(format!("saved queue recall refused: {detail}"))
}

#[derive(PartialEq, Eq)]
struct Group {
    members: Vec<PlayerId>,
    boot_seq: Option<u32>,
}

fn group<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    coordinator: &PlayerId,
) -> Result<Group, ProtoError> {
    let topology = get_zone_group_state(t, host)?;
    let mut groups = topology
        .groups
        .iter()
        .filter(|group| group.coordinator == *coordinator);
    let found = groups
        .next()
        .ok_or_else(|| invalid("the original player no longer coordinates its group"))?;
    if groups.next().is_some()
        || !found
            .members
            .iter()
            .any(|member| member.uuid == *coordinator)
    {
        return Err(invalid("the queue coordinator is ambiguous or absent"));
    }
    let mut members: Vec<_> = found
        .members
        .iter()
        .filter(|member| !member.is_zone_bridge)
        .flat_map(fsonos_proto::topology::ZoneMember::bonded_players)
        .cloned()
        .collect();
    members.sort_by(|a, b| a.0.cmp(&b.0));
    members.dedup();
    let boot_seq = topology
        .boot_seqs()
        .find_map(|(id, boot)| (id == coordinator).then_some(boot));
    Ok(Group { members, boot_seq })
}

pub(super) fn boot<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
) -> Result<Option<u32>, CoreError> {
    Ok(group(t, control::locate(households, coordinator)?.ip, coordinator)?.boot_seq)
}

#[derive(PartialEq, Eq)]
struct Entry {
    uri: String,
    total: u32,
}

fn entry<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    queue: &QueueReference,
) -> Result<Entry, ProtoError> {
    let page = content::browse(
        t,
        host,
        QUEUE,
        BrowseFlag::DirectChildren,
        queue.track - 1,
        1,
    )?;
    if page.update_id != queue.update_id {
        return Err(invalid(
            "the queue changed since the scene was saved; save the scene again",
        ));
    }
    if queue.track > page.total_matches || page.number_returned != 1 || page.objects.len() != 1 {
        return Err(invalid("the saved queue occurrence is missing"));
    }
    let uri = page.objects[0]
        .res
        .as_ref()
        .map(|resource| resource.uri.as_str())
        .filter(|uri| !uri.trim().is_empty())
        .ok_or_else(|| invalid("the saved queue occurrence has no playable URI"))?;
    Ok(Entry {
        uri: uri.into(),
        total: page.total_matches,
    })
}

fn same_track(a: &str, b: &str) -> bool {
    a == b
        || spotify_uri_from_renderer_uri(a).is_some_and(|source| {
            spotify_uri_from_renderer_uri(b).as_deref() == Some(source.as_str())
        })
}

/// Capture only an occupied, selected queue. An empty queue keeps its honest
/// empty-source behavior; a nonempty queue without a selection is not guessed.
pub(super) fn capture<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    snap: &ZoneSnapshot,
    before_boot: Option<u32>,
) -> Result<Option<QueueReference>, CoreError> {
    let started = Instant::now();
    let SnapshotSource::Queue {
        track,
        position_secs,
        update_id,
    } = snap.source
    else {
        return Ok(None);
    };
    let host = control::locate(households, &snap.coordinator)?.ip;
    let head = content::browse(t, host, QUEUE, BrowseFlag::DirectChildren, 0, 1)?;
    if head.update_id != update_id || group(t, host, &snap.coordinator)?.boot_seq != before_boot {
        return Err(invalid("the queue changed or its coordinator rebooted while saving").into());
    }
    if track == 0 && head.total_matches == 0 && head.number_returned == 0 && head.objects.is_empty()
    {
        return Ok(None);
    }
    let queue = QueueReference {
        coordinator: snap.coordinator.clone(),
        household: snap.household.clone(),
        update_id,
        track,
        position_secs,
        play_mode: snap.play_mode,
        boot_seq: before_boot,
    };
    queue.validate().map_err(|why| invalid(&why))?;
    let selected = entry(t, host, &queue)?;
    let position = soap::get_position_info(t, host)?;
    let advance = if snap.transport_state == TransportState::Playing {
        u32::try_from(started.elapsed().as_secs())
            .unwrap_or(u32::MAX)
            .saturating_add(1)
    } else {
        0
    };
    if position.track != track
        || !same_track(&position.uri, &selected.uri)
        || !position
            .position_secs
            .is_some_and(|at| at >= position_secs && at <= position_secs.saturating_add(advance))
        || soap::get_media_info(t, host)?.uri != queue.uri()
        || soap::get_transport_info(t, host)?.state != snap.transport_state
        || queue
            .play_mode
            .is_some_and(|mode| !get_play_mode(t, host).is_ok_and(|actual| actual == mode))
        || entry(t, host, &queue)? != selected
        || group(t, host, &snap.coordinator)?.boot_seq != before_boot
    {
        return Err(invalid("the queue selection changed while saving").into());
    }
    Ok(Some(queue))
}

struct Playback {
    media: MediaInfo,
    position: PositionInfo,
    state: TransportState,
    observed: Instant,
}

impl Playback {
    fn read<T: Transport + ?Sized>(t: &T, host: IpAddr) -> Result<Self, ProtoError> {
        let media = soap::get_media_info(t, host)?;
        let position = soap::get_position_info(t, host)?;
        let state = soap::get_transport_info(t, host)?.state;
        if soap::get_media_info(t, host)?.uri != media.uri {
            return Err(invalid("the source changed during a playback read"));
        }
        Ok(Self {
            media,
            position,
            state,
            observed: Instant::now(),
        })
    }

    fn same_source(&self, other: &Self) -> bool {
        self.media.uri == other.media.uri
            && self.media.uri_metadata == other.media.uri_metadata
            && self.position.track == other.position.track
            && same_track(&self.position.uri, &other.position.uri)
    }

    fn agrees(&self, other: &Self) -> bool {
        let forward = if self.state == TransportState::Playing {
            u32::try_from(self.observed.elapsed().as_secs())
                .unwrap_or(u32::MAX)
                .saturating_add(1)
        } else {
            0
        };
        self.same_source(other)
            && self.state == other.state
            && match (self.position.position_secs, other.position.position_secs) {
                (Some(before), Some(after)) => {
                    after >= before && after <= before.saturating_add(forward)
                }
                (None, None) => true,
                _ => false,
            }
    }
}

/// All mutations made by snapshot restore, including its internal mode write,
/// pass this fence. Reads used by the fence use the underlying transport.
struct Guarded<'a, T: ?Sized> {
    t: &'a T,
    host: IpAddr,
    queue: &'a QueueReference,
    group: Group,
    entry: Entry,
    expected: RefCell<Playback>,
    expected_mode: RefCell<Option<PlayMode>>,
    poisoned: Cell<bool>,
}

impl<'a, T: Transport + ?Sized> Guarded<'a, T> {
    fn prepare(
        t: &'a T,
        house: &HouseholdState,
        queue: &'a QueueReference,
    ) -> Result<Self, CoreError> {
        let coordinator = &queue.coordinator;
        let host = control::locate(std::slice::from_ref(house), coordinator)?.ip;
        let current_group = group(t, host, coordinator)?;
        let mut expected_members = house
            .groups
            .iter()
            .find(|group| group.coordinator == *coordinator)
            .map(|group| group.members.clone())
            .ok_or_else(|| invalid("the original player no longer leads the intended group"))?;
        expected_members.sort_by(|a, b| a.0.cmp(&b.0));
        expected_members.dedup();
        if current_group.members != expected_members {
            return Err(invalid("the intended group membership changed before recall").into());
        }
        if queue
            .boot_seq
            .is_some_and(|saved| current_group.boot_seq != Some(saved))
        {
            return Err(invalid("the original coordinator rebooted; save the scene again").into());
        }
        let selected = entry(t, host, queue)?;
        let current = Playback::read(t, host)?;
        let current_mode = queue
            .play_mode
            .map(|_| get_play_mode(t, host))
            .transpose()?;
        Ok(Self {
            t,
            host,
            queue,
            group: current_group,
            entry: selected,
            expected: RefCell::new(current),
            expected_mode: RefCell::new(current_mode),
            poisoned: Cell::new(false),
        })
    }

    /// Snapshot source recovery may reconcile an ambiguous network reply,
    /// but it must never turn a failed ownership fence into permission for
    /// another write. Only a successful proof keeps this guard usable.
    fn fenced(&self, check: impl FnOnce() -> Result<(), ProtoError>) -> Result<(), ProtoError> {
        if self.poisoned.get() {
            return Err(invalid(
                "an earlier playback fence failed; no further writes are allowed",
            ));
        }
        let result = check();
        if result.is_err() {
            self.poisoned.set(true);
        }
        result
    }

    fn epoch(&self) -> Result<(), ProtoError> {
        if group(self.t, self.host, &self.queue.coordinator)? != self.group {
            return Err(invalid("the group or coordinator boot sequence changed"));
        }
        if entry(self.t, self.host, self.queue)? != self.entry {
            return Err(invalid("the queue occurrence or length changed"));
        }
        Ok(())
    }

    fn before(&self) -> Result<(), ProtoError> {
        self.fenced(|| {
            self.epoch()?;
            if !self
                .expected
                .borrow()
                .agrees(&Playback::read(self.t, self.host)?)
            {
                return Err(invalid(
                    "the source, selected occurrence, time or transport changed during recall",
                ));
            }
            if let Some(expected) = *self.expected_mode.borrow()
                && get_play_mode(self.t, self.host)? != expected
            {
                return Err(invalid("the queue play mode changed during recall"));
            }
            self.epoch()
        })
    }

    fn selected(&self, playback: &Playback) -> bool {
        playback.media.uri == self.queue.uri()
            && playback.position.track == self.queue.track
            && same_track(&playback.position.uri, &self.entry.uri)
    }

    fn at_position(&self, playback: &Playback) -> bool {
        playback.position.position_secs.is_some_and(|position| {
            if playback.state == TransportState::Playing {
                position >= self.queue.position_secs
                    && position <= self.queue.position_secs.saturating_add(1)
            } else {
                position == self.queue.position_secs
            }
        })
    }

    fn after(&self, action: &str, body: &str) -> Result<(), ProtoError> {
        self.fenced(|| {
            self.epoch()?;
            let now = Playback::read(self.t, self.host)?;
            let mode = self
                .expected_mode
                .borrow()
                .map(|_| get_play_mode(self.t, self.host))
                .transpose()?;
            let before = self.expected.borrow();
            let quiet = matches!(now.state, TransportState::Paused | TransportState::Stopped);
            let matched = match action {
                "SetAVTransportURI" => now.media.uri == self.queue.uri() && quiet,
                "SetPlayMode" => before.agrees(&now) && mode == self.queue.play_mode,
                "Seek" if body.contains("<Unit>TRACK_NR</Unit>") => {
                    self.selected(&now)
                        && now.state == before.state
                        && now.position.position_secs.is_some_and(|at| at <= 1)
                }
                "Seek" => {
                    self.selected(&now) && self.at_position(&now) && now.state == before.state
                }
                "Play" => {
                    self.selected(&now)
                        && self.at_position(&now)
                        && now.state == TransportState::Playing
                }
                "Pause" | "Stop" => before.same_source(&now) && quiet,
                _ => false,
            };
            let mode_matches = matches!(action, "SetAVTransportURI" | "SetPlayMode")
                || mode == *self.expected_mode.borrow();
            if !matched || !mode_matches {
                return Err(invalid(
                    "the requested queue playback step was not confirmed; no write was repeated",
                ));
            }
            drop(before);
            self.epoch()?;
            *self.expected.borrow_mut() = now;
            *self.expected_mode.borrow_mut() = mode;
            Ok(())
        })
    }
}

impl<T: Transport + ?Sized> Transport for Guarded<'_, T> {
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
        if name.starts_with("Get") || name == "Browse" {
            return self.t.soap_post(host, path, action, body);
        }
        if host != self.host
            || !matches!(
                name,
                "SetAVTransportURI" | "SetPlayMode" | "Seek" | "Play" | "Pause" | "Stop"
            )
        {
            return Err(invalid("an unexpected mutation was requested"));
        }
        self.before()?;
        let written = self.t.soap_post(host, path, action, body);
        // A definite SOAP refusal did not authorize another state transition.
        if written.as_ref().is_ok_and(|body| {
            matches!(
                fsonos_proto::soap::parse_response(body, name),
                Err(ProtoError::SoapFault { .. })
            )
        }) {
            return written;
        }
        let proof = self.after(name, body);
        match (written, proof) {
            (Ok(response), Ok(())) => Ok(response),
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        }
    }
}

/// Return whether any playback write was needed. An unchanged queue is still
/// checked for coordinator reboot before it can be reported as a no-op.
pub(super) fn restore<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    queue: &QueueReference,
    start: bool,
) -> Result<bool, String> {
    queue.validate()?;
    if coordinator != &queue.coordinator {
        return Err(
            "the saved queue's original coordinator differs from this room's player".into(),
        );
    }
    let house = households
        .iter()
        .find(|house| house.player(coordinator).is_some())
        .ok_or_else(|| "the original queue coordinator is missing".to_string())?;
    if queue
        .household
        .as_ref()
        .is_some_and(|saved| house.id.as_ref() != Some(saved))
    {
        return Err("the saved queue belongs to a different household".into());
    }
    let result = (|| -> Result<bool, CoreError> {
        let guarded = Guarded::prepare(t, house, queue)?;
        let host = guarded.host;
        guarded.before()?;
        let current = guarded.expected.borrow();
        let state_matches = if start {
            current.state == TransportState::Playing
        } else {
            matches!(
                current.state,
                TransportState::Paused | TransportState::Stopped
            )
        };
        let unchanged = guarded.selected(&current)
            && guarded.at_position(&current)
            && *guarded.expected_mode.borrow() == queue.play_mode
            && state_matches;
        drop(current);
        if unchanged {
            return Ok(false);
        }
        if !start {
            control::pause_confirmed(&guarded, households, coordinator)?;
        }
        let snap = ZoneSnapshot {
            household: queue.household.clone(),
            coordinator: coordinator.clone(),
            members: Vec::new(),
            levels: Vec::new(),
            group_volume: None,
            transport_state: TransportState::Stopped,
            play_mode: queue.play_mode,
            source: SnapshotSource::Queue {
                track: queue.track,
                position_secs: queue.position_secs,
                update_id: queue.update_id,
            },
            captured_at: 0,
        };
        let report = snapshot::restore_only(
            &guarded,
            households,
            &snap,
            &[Aspect::Source, Aspect::Position],
        )?;
        if !report.skipped.is_empty() {
            return Err(invalid(
                &report
                    .skipped
                    .iter()
                    .map(|(_, why)| why.as_str())
                    .collect::<Vec<_>>()
                    .join("; "),
            )
            .into());
        }
        // Generic snapshots omit a zero-time seek. A bookmark explicitly
        // names the beginning too, including when its track was already on.
        if queue.position_secs == 0 {
            soap::seek_position(&guarded, host, 0)?;
        }
        if start {
            soap::play(&guarded, host)?;
        }
        guarded.before()?;
        Ok(true)
    })();
    result.map_err(|error| error.to_string())
}
