//! Durable evidence for one sleep fade, separate from its armed timer.
//! A pending level is published before the speaker write: after a crash either
//! the last confirmed level or that pending level can still belong to the fade.

use super::{CoreError, Fader, HouseholdState, PlayerId, SleepTimer, Transport, ip, soap};
use crate::snapshot::{self, SnapshotSource, ZoneSnapshot};
use chrono::{DateTime, Utc};
use fsonos_proto::ProtoError;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_types::TransportState;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// The daemon persists these callbacks atomically before allowing a write.
/// Direct callers without persistence keep the ordinary sleep path.
pub trait SleepCheckpoint {
    fn save(&self, fade: &SleepFade) -> Result<(), CoreError>;
    fn complete(&self, fade: &SleepFade) -> Result<(), CoreError>;
    /// A newer local intent may retire a startup recovery during its reads.
    fn current(&self, fade: &SleepFade) -> bool;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Level {
    player: PlayerId,
    original: u8,
    confirmed: u8,
    pending: Option<u8>,
}

impl Level {
    fn owns(&self, level: u8) -> bool {
        level == self.confirmed || self.pending == Some(level)
    }
}

/// Private saved-file data, not a general-purpose playback snapshot to replay.
/// Recovery only changes still-owned volumes; it never replays a source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SleepFade {
    pub id: String,
    pub timer: SleepTimer,
    before: ZoneSnapshot,
    /// Physical identities and their boot observations, sorted by UUID.
    physical: Vec<(PlayerId, Option<u32>)>,
    /// Bonding must remain unchanged, even when the room name is unchanged.
    rooms: Vec<(PlayerId, Vec<PlayerId>)>,
    levels: Vec<Level>,
    finishing: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SleepRecovery {
    /// The matching backstop still runs: continue with the original baseline.
    Resume,
    /// Finish only volume cleanup; never replay a possibly accepted stop.
    Restore,
    /// A new source, grouping, timer or playback intent owns the room now.
    Changed,
}

pub(super) fn changed() -> CoreError {
    ProtoError::Malformed("the sleep fade's group or playback changed".into()).into()
}

fn source_matches(a: &SnapshotSource, b: &SnapshotSource) -> bool {
    match (a, b) {
        (SnapshotSource::Nothing, SnapshotSource::Nothing) => true,
        (
            SnapshotSource::Queue { update_id: a, .. },
            SnapshotSource::Queue { update_id: b, .. },
        ) => *a != 0 && a == b,
        (
            SnapshotSource::Uri {
                uri: a,
                metadata: am,
                ..
            },
            SnapshotSource::Uri {
                uri: b,
                metadata: bm,
                ..
            },
        ) => a == b && am == bm,
        _ => false,
    }
}

type Physical = Vec<(PlayerId, Option<u32>)>;
type Rooms = Vec<(PlayerId, Vec<PlayerId>)>;

fn layout<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
) -> Result<Option<(HouseholdState, Physical, Rooms)>, CoreError> {
    let known = households
        .iter()
        .find(|h| h.player(coordinator).is_some())
        .ok_or_else(|| CoreError::UnknownPlayer(coordinator.0.clone()))?;
    let topology = get_zone_group_state(t, ip(households, coordinator)?)?;
    let mut groups = topology
        .groups
        .iter()
        .filter(|g| g.coordinator == *coordinator);
    let Some(group) = groups.next() else {
        return Ok(None);
    };
    if groups.next().is_some() {
        return Err(changed());
    }
    let mut physical: Physical = group
        .members
        .iter()
        .flat_map(|m| std::iter::once(m).chain(&m.satellites))
        .map(|m| (m.uuid.clone(), m.boot_seq))
        .collect();
    physical.sort_by(|a, b| a.0.0.cmp(&b.0.0));
    if !physical.iter().any(|(p, _)| p == coordinator)
        || physical.windows(2).any(|pair| pair[0].0 == pair[1].0)
        || topology
            .groups
            .iter()
            .filter(|g| g.coordinator != *coordinator)
            .any(|g| {
                g.members
                    .iter()
                    .flat_map(|m| std::iter::once(m).chain(&m.satellites))
                    .any(|m| physical.iter().any(|(p, _)| *p == m.uuid))
            })
    {
        return Err(changed());
    }
    let mut fresh = known.clone();
    fresh.apply_topology(&topology);
    let mut rooms: Rooms = fresh
        .rooms
        .iter()
        .filter(|r| r.coordinator == *coordinator)
        .map(|r| {
            let mut players = r.players.clone();
            players.sort_by(|a, b| a.0.cmp(&b.0));
            (r.primary.clone(), players)
        })
        .collect();
    rooms.sort_by(|a, b| a.0.0.cmp(&b.0.0));
    Ok(Some((fresh, physical, rooms)))
}

impl SleepFade {
    pub(crate) fn capture<T: Transport + ?Sized>(
        t: &T,
        households: &[HouseholdState],
        timer: SleepTimer,
        claimed: &[(PlayerId, u64)],
    ) -> Result<Self, CoreError> {
        let coordinator = households
            .iter()
            .find_map(|h| h.coordinator_of(&timer.coordinator))
            .ok_or_else(changed)?;
        let (fresh, physical, rooms) = layout(t, households, coordinator)?.ok_or_else(changed)?;
        let before = snapshot::capture(t, std::slice::from_ref(&fresh), coordinator, 0)?;
        let levels = claimed
            .iter()
            .map(|(player, _)| {
                let level = before
                    .levels
                    .iter()
                    .find(|level| level.player == *player)
                    .ok_or_else(changed)?;
                Ok(Level {
                    player: player.clone(),
                    original: level.volume,
                    confirmed: level.volume,
                    pending: None,
                })
            })
            .collect::<Result<Vec<_>, CoreError>>()?;
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let fade = Self {
            id: format!("{stamp}-{}", NEXT.fetch_add(1, Ordering::Relaxed)),
            timer,
            before,
            physical,
            rooms,
            levels,
            finishing: false,
        };
        if !fade.valid() || fade.observed(t, households)?.is_none() {
            return Err(changed());
        }
        Ok(fade)
    }

    /// Reject corrupted/ambiguous journals before they authorize any recovery.
    #[must_use]
    pub fn valid(&self) -> bool {
        !self.id.is_empty()
            && !self.timer.coordinator.0.is_empty()
            && !self.levels.is_empty()
            && source_matches(&self.before.source, &self.before.source)
            && self
                .physical
                .iter()
                .any(|(p, _)| *p == self.before.coordinator)
            && self.physical.windows(2).all(|p| p[0].0.0 < p[1].0.0)
            && !self.rooms.is_empty()
            && self.rooms.windows(2).all(|p| p[0].0.0 < p[1].0.0)
            && self.rooms.iter().all(|(primary, members)| {
                members.contains(primary)
                    && members.windows(2).all(|p| p[0].0 < p[1].0)
                    && members
                        .iter()
                        .all(|p| self.physical.iter().any(|(physical, _)| physical == p))
            })
            && self.levels.iter().all(|level| {
                level.original <= 100
                    && level.confirmed <= 100
                    && level.pending.is_none_or(|p| p <= 100)
                    && self
                        .before
                        .levels
                        .iter()
                        .any(|old| old.player == level.player && old.volume == level.original)
                    && self
                        .levels
                        .iter()
                        .filter(|other| other.player == level.player)
                        .count()
                        == 1
                    && self
                        .rooms
                        .iter()
                        .any(|(primary, _)| *primary == level.player)
            })
    }

    pub(crate) fn original_levels(&self) -> Vec<(PlayerId, u8)> {
        self.levels
            .iter()
            .map(|l| (l.player.clone(), l.original))
            .collect()
    }

    pub(crate) fn matches_claims(&self, claimed: &[(PlayerId, u64)]) -> bool {
        claimed.len() == self.levels.len()
            && claimed
                .iter()
                .all(|(player, _)| self.levels.iter().any(|l| l.player == *player))
    }

    /// A local command on the recorded group invalidates its recovery intent.
    #[must_use]
    pub fn affects(&self, coordinator: &PlayerId) -> bool {
        self.timer.coordinator == *coordinator || self.before.coordinator == *coordinator
    }

    fn observed<T: Transport + ?Sized>(
        &self,
        t: &T,
        households: &[HouseholdState],
    ) -> Result<Option<ZoneSnapshot>, CoreError> {
        let Some((fresh, physical, rooms)) = layout(t, households, &self.before.coordinator)?
        else {
            return Ok(None);
        };
        if physical != self.physical || rooms != self.rooms {
            return Ok(None);
        }
        let now = snapshot::capture(t, std::slice::from_ref(&fresh), &self.before.coordinator, 0)?;
        let same_members = now.members.len() == self.before.members.len()
            && now.members.iter().all(|p| self.before.members.contains(p));
        let same_layout = layout(t, households, &self.before.coordinator)?
            .is_some_and(|(_, physical, rooms)| physical == self.physical && rooms == self.rooms);
        Ok((same_layout
            && same_members
            && source_matches(&self.before.source, &now.source)
            && now.household == self.before.household)
            .then_some(now))
    }

    /// Read-only startup decision. Missing reads remain pending; changed state
    /// is not permission to restart playback or repeat a stop.
    pub fn recovery<T: Transport + ?Sized>(
        &self,
        t: &T,
        households: &[HouseholdState],
        now: DateTime<Utc>,
        backstop_matches: bool,
    ) -> Result<SleepRecovery, CoreError> {
        let Some(current) = self.observed(t, households)? else {
            return Ok(SleepRecovery::Changed);
        };
        let inactive = matches!(
            current.transport_state,
            TransportState::Paused | TransportState::Stopped
        );
        let unchanged = current.transport_state == self.before.transport_state;
        let owned = self.levels.iter().all(|l| {
            current
                .levels
                .iter()
                .any(|now| now.player == l.player && l.owns(now.volume))
        });
        if self.finishing && (unchanged || inactive) {
            return Ok(SleepRecovery::Restore);
        }
        if backstop_matches && unchanged {
            return Ok(if owned {
                SleepRecovery::Resume
            } else {
                SleepRecovery::Restore
            });
        }
        Ok(if unchanged || (now >= self.timer.ends && inactive) {
            SleepRecovery::Restore
        } else {
            SleepRecovery::Changed
        })
    }

    pub(crate) fn step(
        &mut self,
        checkpoint: &dyn SleepCheckpoint,
        player: &PlayerId,
        confirmed: u8,
        pending: u8,
    ) -> Result<(), CoreError> {
        let level = self
            .levels
            .iter_mut()
            .find(|l| l.player == *player)
            .ok_or_else(changed)?;
        if !level.owns(confirmed) {
            return Err(changed());
        }
        level.confirmed = confirmed;
        level.pending = Some(pending);
        checkpoint.save(self)
    }

    pub(crate) fn stopping<T: Transport + ?Sized>(
        &mut self,
        t: &T,
        households: &[HouseholdState],
        checkpoint: &dyn SleepCheckpoint,
    ) -> Result<(), CoreError> {
        let Some(now) = self.observed(t, households)? else {
            return Err(changed());
        };
        if now.transport_state != self.before.transport_state
            || !checkpoint.current(self)
            || !self.levels.iter().all(|l| {
                now.levels
                    .iter()
                    .any(|now| now.player == l.player && l.owns(now.volume))
            })
        {
            return Err(changed());
        }
        self.finishing = true;
        checkpoint.save(self)
    }

    /// A joined room's successful leave changes its source and group. Bind the
    /// cleanup to that observed result; never infer it after an uncertain leave.
    pub(crate) fn left<T: Transport + ?Sized>(
        &mut self,
        t: &T,
        households: &[HouseholdState],
        checkpoint: &dyn SleepCheckpoint,
    ) -> Result<(), CoreError> {
        let (fresh, physical, rooms) =
            layout(t, households, &self.timer.coordinator)?.ok_or_else(changed)?;
        let mut before =
            snapshot::capture(t, std::slice::from_ref(&fresh), &self.timer.coordinator, 0)?;
        for level in &mut before.levels {
            if let Some(original) = self.levels.iter().find(|l| l.player == level.player) {
                level.volume = original.original;
            }
        }
        self.before = before;
        self.physical = physical;
        self.rooms = rooms;
        checkpoint.save(self)
    }

    /// Restore only matching levels while keeping the existing fader claims.
    /// Returning success also covers a newer owner that must be left alone.
    pub(crate) fn restore_claimed<T: Transport + ?Sized>(
        &mut self,
        t: &T,
        households: &[HouseholdState],
        fader: &Fader,
        claimed: &[(PlayerId, u64)],
        checkpoint: &dyn SleepCheckpoint,
    ) -> Result<(), CoreError> {
        let Some(current) = self.observed(t, households)? else {
            return Ok(());
        };
        if !checkpoint.current(self)
            || !(current.transport_state == self.before.transport_state
                || matches!(
                    current.transport_state,
                    TransportState::Paused | TransportState::Stopped
                ))
        {
            return Ok(());
        }
        // Cleanup must never be mistaken for permission to resume the fade if
        // publication of its completed state fails or the process stops here.
        self.finishing = true;
        checkpoint.save(self)?;
        let mut first = Ok(());
        for (player, intent) in claimed {
            let done = fader
                .with_claimed(&[(player.clone(), *intent)], || {
                    if !checkpoint.current(self) {
                        return Ok(());
                    }
                    let Some(level) = self.levels.iter().find(|l| l.player == *player).cloned()
                    else {
                        return Err(changed());
                    };
                    let host = ip(households, player)?;
                    let current = soap::get_volume(t, host)?;
                    if current == level.original || !level.owns(current) {
                        return Ok(());
                    }
                    self.step(checkpoint, player, current, level.original)?;
                    soap::set_volume(t, host, level.original).map_err(CoreError::from)
                })
                .unwrap_or(Ok(()));
            if first.is_ok() {
                first = done;
            }
        }
        first
    }

    /// Startup cleanup reserves ordinary volume intents and never sends Pause,
    /// Play, a queue edit or a grouping action.
    pub fn restore<T: Transport + ?Sized>(
        &mut self,
        t: &T,
        households: &[HouseholdState],
        fader: &Fader,
        checkpoint: &dyn SleepCheckpoint,
    ) -> Result<(), CoreError> {
        if !checkpoint.current(self) {
            return Ok(());
        }
        let players: Vec<_> = self.levels.iter().map(|l| l.player.clone()).collect();
        let claimed = fader.claim(&players);
        self.restore_claimed(t, households, fader, &claimed, checkpoint)?;
        checkpoint.complete(self)
    }
}
