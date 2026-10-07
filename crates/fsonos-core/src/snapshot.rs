//! Zone snapshots: capture what a zone is doing and put it back.
//!
//! Undo, scenes, announcements and test plays share this one model. A
//! snapshot records the group (its coordinator and member rooms), each member
//! room's own volume and mute, what the coordinator plays (its queue at a
//! track and position, or a single URI), and whether it was playing.
//!
//! Restore is planned as a pure list of operations ([`plan_restore`]) and run
//! in order: regroup, set the source, seek, restore volumes and mute, then play
//! only if it was playing. Member volumes go back one by one with SetVolume,
//! never through SetGroupVolume (which scales members proportionally). The
//! queue's contents are not captured, only its reference: if the queue changed
//! since (its ContentDirectory `UpdateID` moved), the source and position are
//! reported as not restorable rather than resuming something else. Play mode
//! (shuffle / repeat) is not captured yet: there is no read for it.

use crate::{CoreError, HouseholdState};
use fsonos_proto::content::{self, BrowseFlag};
use fsonos_proto::{Transport, control as soap};
use fsonos_types::{HouseholdId, PlayerId, TransportState};
use std::net::IpAddr;

/// One member room's own levels (addressed through the room's primary).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MemberLevel {
    pub player: PlayerId,
    pub volume: u8,
    pub mute: bool,
}

/// What the coordinator was playing from.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SnapshotSource {
    /// Nothing loaded.
    Nothing,
    /// Its own queue, at `track` (1-based) and `position_secs` into it.
    /// `update_id` identifies the queue's contents at capture.
    Queue {
        track: u32,
        position_secs: u32,
        update_id: u32,
    },
    /// A single URI (a stream or one track) and its metadata; the position
    /// only when the item has a duration (streams have none).
    Uri {
        uri: String,
        metadata: String,
        position_secs: Option<u32>,
    },
}

/// What a zone was doing at `captured_at`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ZoneSnapshot {
    pub household: Option<HouseholdId>,
    pub coordinator: PlayerId,
    /// The primary player of every room in the group, the coordinator's first.
    pub members: Vec<PlayerId>,
    pub levels: Vec<MemberLevel>,
    /// For reference only; restore never sets it (see the module docs).
    pub group_volume: Option<u8>,
    pub transport_state: TransportState,
    pub source: SnapshotSource,
    /// Unix seconds, stamped by the caller.
    pub captured_at: i64,
}

/// One step of a restore, in the order they run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreOp {
    /// Take `player` out of whatever group it is in.
    Leave {
        player: PlayerId,
    },
    /// Put `player` (a room's primary) in the coordinator's group.
    Join {
        player: PlayerId,
    },
    /// Make the coordinator play its own queue.
    PlayQueue,
    SetUri {
        uri: String,
        metadata: String,
    },
    SeekTrack(u32),
    SeekPosition(u32),
    SetVolume {
        player: PlayerId,
        level: u8,
    },
    SetMute {
        player: PlayerId,
        mute: bool,
    },
    Play,
    Stop,
}

/// The parts of a snapshot a restore puts back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Aspect {
    Group,
    Source,
    Position,
    Volume,
    Mute,
    Transport,
}

/// What a restore put back, and what it could not, with why.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RestoreReport {
    pub restored: Vec<Aspect>,
    pub skipped: Vec<(Aspect, String)>,
}

/// Capture what the group `coordinator` leads is doing now.
pub fn capture<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    captured_at: i64,
) -> Result<ZoneSnapshot, CoreError> {
    let household = households
        .iter()
        .find(|h| h.player(coordinator).is_some())
        .ok_or_else(|| CoreError::UnknownPlayer(coordinator.0.clone()))?;
    let host = ip(household, coordinator)?;
    let members = member_rooms(household, coordinator);
    let transport = soap::get_transport_info(t, host)?;
    let position = soap::get_position_info(t, host)?;
    let media = soap::get_media_info(t, host)?;
    let source = if media.uri.starts_with("x-rincon-queue:") {
        SnapshotSource::Queue {
            track: position.track,
            position_secs: position.position_secs.unwrap_or(0),
            update_id: queue_update_id(t, host)?,
        }
    } else if media.uri.is_empty() {
        SnapshotSource::Nothing
    } else {
        let has_duration = position.duration_secs.is_some_and(|d| d > 0);
        SnapshotSource::Uri {
            uri: media.uri,
            metadata: media.uri_metadata,
            position_secs: position.position_secs.filter(|_| has_duration),
        }
    };
    let levels = members
        .iter()
        .map(|m| {
            let at = ip(household, m)?;
            Ok(MemberLevel {
                player: m.clone(),
                volume: soap::get_volume(t, at)?,
                mute: soap::get_mute(t, at)?,
            })
        })
        .collect::<Result<_, CoreError>>()?;
    Ok(ZoneSnapshot {
        household: household.id.clone(),
        coordinator: coordinator.clone(),
        members,
        levels,
        group_volume: soap::get_group_volume(t, host).ok(),
        transport_state: transport.state,
        source,
        captured_at,
    })
}

/// The primary of each room in the group `coordinator` leads, its own first.
pub(crate) fn member_rooms(household: &HouseholdState, coordinator: &PlayerId) -> Vec<PlayerId> {
    let mut rooms: Vec<&crate::Room> = household
        .rooms
        .iter()
        .filter(|r| r.coordinator == *coordinator)
        .collect();
    rooms.sort_by_key(|r| !r.players.contains(coordinator));
    let members: Vec<PlayerId> = rooms.iter().map(|r| r.primary.clone()).collect();
    if members.is_empty() {
        vec![coordinator.clone()]
    } else {
        members
    }
}

fn ip(household: &HouseholdState, player: &PlayerId) -> Result<IpAddr, CoreError> {
    household
        .player(player)
        .map(|p| p.ip)
        .ok_or_else(|| CoreError::UnknownPlayer(player.0.clone()))
}

/// The coordinator's queue `UpdateID`: it changes whenever the queue does.
fn queue_update_id<T: Transport + ?Sized>(t: &T, host: IpAddr) -> Result<u32, CoreError> {
    Ok(content::browse(t, host, content::QUEUE, BrowseFlag::DirectChildren, 0, 1)?.update_id)
}

/// The operations that put a snapshot back, each tagged with the aspect it
/// restores, and the aspects that cannot be restored, with why.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestorePlan {
    pub ops: Vec<(Aspect, RestoreOp)>,
    pub skipped: Vec<(Aspect, String)>,
}

impl RestorePlan {
    fn op(&mut self, aspect: Aspect, op: RestoreOp) {
        self.ops.push((aspect, op));
    }

    fn skip(&mut self, aspect: Aspect, why: impl Into<String>) {
        self.skipped.push((aspect, why.into()));
    }
}

/// Plan the restore of `snap`, given the household as it is now and the
/// coordinator's current queue `UpdateID` (for queue sources).
#[must_use]
pub fn plan_restore(
    snap: &ZoneSnapshot,
    household: &HouseholdState,
    queue_update_id: Option<u32>,
) -> RestorePlan {
    let mut plan = RestorePlan::default();
    if household.player(&snap.coordinator).is_none() {
        let why = format!("coordinator {} is gone", snap.coordinator.0);
        for a in [
            Aspect::Group,
            Aspect::Source,
            Aspect::Position,
            Aspect::Volume,
            Aspect::Mute,
            Aspect::Transport,
        ] {
            plan.skip(a, why.clone());
        }
        return plan;
    }
    plan_group(snap, household, &mut plan);
    let source_restored = plan_source(snap, queue_update_id, &mut plan);
    plan_levels(snap, household, &mut plan);
    plan_transport(snap, source_restored, &mut plan);
    plan
}

/// 1. Regroup: the coordinator leads, the snapshot's rooms are with it, and
///    no other room is.
fn plan_group(snap: &ZoneSnapshot, household: &HouseholdState, plan: &mut RestorePlan) {
    let coordinator = &snap.coordinator;
    if household.coordinator_of(coordinator) != Some(coordinator) {
        plan.op(
            Aspect::Group,
            RestoreOp::Leave {
                player: coordinator.clone(),
            },
        );
    }
    for r in household
        .rooms
        .iter()
        .filter(|r| r.coordinator == *coordinator)
    {
        if !r.players.contains(coordinator) && !snap.members.contains(&r.primary) {
            plan.op(
                Aspect::Group,
                RestoreOp::Leave {
                    player: r.primary.clone(),
                },
            );
        }
    }
    for m in snap.members.iter().filter(|m| *m != coordinator) {
        match household.rooms.iter().find(|r| r.primary == *m) {
            None => plan.skip(Aspect::Group, format!("room of {} is gone", m.0)),
            Some(r) if r.coordinator == *coordinator => {}
            Some(_) => plan.op(Aspect::Group, RestoreOp::Join { player: m.clone() }),
        }
    }
}

/// 2–3. The source, then the position within it. Returns whether the source
/// can be restored.
fn plan_source(snap: &ZoneSnapshot, queue_update_id: Option<u32>, plan: &mut RestorePlan) -> bool {
    match &snap.source {
        SnapshotSource::Nothing => true,
        SnapshotSource::Queue {
            track,
            position_secs,
            update_id,
        } => {
            if queue_update_id != Some(*update_id) {
                let why = "the queue changed since the snapshot";
                plan.skip(Aspect::Source, why);
                plan.skip(Aspect::Position, why);
                return false;
            }
            plan.op(Aspect::Source, RestoreOp::PlayQueue);
            if *track > 0 {
                plan.op(Aspect::Position, RestoreOp::SeekTrack(*track));
            }
            if *position_secs > 0 {
                plan.op(Aspect::Position, RestoreOp::SeekPosition(*position_secs));
            }
            true
        }
        SnapshotSource::Uri {
            uri,
            metadata,
            position_secs,
        } => {
            plan.op(
                Aspect::Source,
                RestoreOp::SetUri {
                    uri: uri.clone(),
                    metadata: metadata.clone(),
                },
            );
            if let Some(p) = position_secs.filter(|p| *p > 0) {
                plan.op(Aspect::Position, RestoreOp::SeekPosition(p));
            }
            true
        }
    }
}

/// 4. Each member room's own volume and mute (never the group volume).
fn plan_levels(snap: &ZoneSnapshot, household: &HouseholdState, plan: &mut RestorePlan) {
    for level in &snap.levels {
        if household.player(&level.player).is_none() {
            plan.skip(Aspect::Volume, format!("{} is gone", level.player.0));
            continue;
        }
        plan.op(
            Aspect::Volume,
            RestoreOp::SetVolume {
                player: level.player.clone(),
                level: level.volume,
            },
        );
        plan.op(
            Aspect::Mute,
            RestoreOp::SetMute {
                player: level.player.clone(),
                mute: level.mute,
            },
        );
    }
}

/// 5. Play only if it was playing and its source came back.
fn plan_transport(snap: &ZoneSnapshot, source_restored: bool, plan: &mut RestorePlan) {
    match (snap.transport_state, source_restored) {
        (TransportState::Playing, true) => plan.op(Aspect::Transport, RestoreOp::Play),
        (TransportState::Playing, false) => plan.skip(
            Aspect::Transport,
            "not resumed: its source could not be restored",
        ),
        // Setting a source leaves the transport stopped; with no source to
        // set, stop explicitly.
        _ if matches!(snap.source, SnapshotSource::Nothing) => {
            plan.op(Aspect::Transport, RestoreOp::Stop);
        }
        _ => {}
    }
}

/// Put `snap` back. Runs every planned operation, carrying on past one that
/// fails; the report says which aspects came back and which did not.
pub fn restore<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    snap: &ZoneSnapshot,
) -> Result<RestoreReport, CoreError> {
    let Some(household) = households
        .iter()
        .find(|h| h.player(&snap.coordinator).is_some())
    else {
        return Ok(RestoreReport {
            restored: Vec::new(),
            skipped: plan_restore(snap, &HouseholdState::default(), None).skipped,
        });
    };
    let host = ip(household, &snap.coordinator)?;
    let update_id = match snap.source {
        SnapshotSource::Queue { .. } => queue_update_id(t, host).ok(),
        _ => None,
    };
    let RestorePlan { ops, mut skipped } = plan_restore(snap, household, update_id);
    let mut restored: Vec<Aspect> = Vec::new();
    let mut failed: Vec<Aspect> = skipped.iter().map(|(a, _)| *a).collect();
    for (aspect, op) in ops {
        match run(t, household, host, &snap.coordinator, &op) {
            Ok(()) => {
                if !restored.contains(&aspect) && !failed.contains(&aspect) {
                    restored.push(aspect);
                }
            }
            Err(e) => {
                restored.retain(|a| *a != aspect);
                failed.push(aspect);
                skipped.push((aspect, format!("{op:?}: {e}")));
            }
        }
    }
    Ok(RestoreReport { restored, skipped })
}

fn run<T: Transport + ?Sized>(
    t: &T,
    household: &HouseholdState,
    host: IpAddr,
    coordinator: &PlayerId,
    op: &RestoreOp,
) -> Result<(), CoreError> {
    match op {
        RestoreOp::Leave { player } => soap::leave_group(t, ip(household, player)?)?,
        RestoreOp::Join { player } => soap::join_group(t, ip(household, player)?, coordinator)?,
        RestoreOp::PlayQueue => soap::play_from_queue(t, host, coordinator)?,
        RestoreOp::SetUri { uri, metadata } => soap::set_av_transport_uri(t, host, uri, metadata)?,
        RestoreOp::SeekTrack(n) => soap::seek_track(t, host, *n)?,
        RestoreOp::SeekPosition(s) => soap::seek_position(t, host, *s)?,
        RestoreOp::SetVolume { player, level } => {
            soap::set_volume(t, ip(household, player)?, *level)?;
        }
        RestoreOp::SetMute { player, mute } => soap::set_mute(t, ip(household, player)?, *mute)?,
        RestoreOp::Play => soap::play(t, host)?,
        RestoreOp::Stop => soap::stop(t, host)?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Room;
    use fsonos_types::{Generation, Player, ZoneGroup};

    fn pid(s: &str) -> PlayerId {
        PlayerId(s.into())
    }

    /// Rooms A, B, C (one player each), grouped as `groups`.
    fn household(groups: &[(&str, &[&str])]) -> HouseholdState {
        let player = |id: &str| Player {
            id: pid(id),
            room_name: id.into(),
            ip: "192.0.2.1".parse().unwrap(),
            model: String::new(),
            generation: Generation::S2,
        };
        let mut st = HouseholdState {
            players: ["A", "B", "C"].iter().map(|id| player(id)).collect(),
            ..Default::default()
        };
        for (coord, members) in groups {
            st.groups.push(ZoneGroup {
                coordinator: pid(coord),
                members: members.iter().map(|m| pid(m)).collect(),
            });
            for m in *members {
                st.rooms.push(Room {
                    name: (*m).into(),
                    primary: pid(m),
                    players: vec![pid(m)],
                    missing: Vec::new(),
                    coordinator: pid(coord),
                });
            }
        }
        st
    }

    fn snap(members: &[&str], source: SnapshotSource, state: TransportState) -> ZoneSnapshot {
        ZoneSnapshot {
            household: None,
            coordinator: pid("A"),
            members: members.iter().map(|m| pid(m)).collect(),
            levels: members
                .iter()
                .map(|m| MemberLevel {
                    player: pid(m),
                    volume: 30,
                    mute: false,
                })
                .collect(),
            group_volume: Some(30),
            transport_state: state,
            source,
            captured_at: 0,
        }
    }

    fn queue(update_id: u32) -> SnapshotSource {
        SnapshotSource::Queue {
            track: 2,
            position_secs: 40,
            update_id,
        }
    }

    fn ops_of(plan: &[(Aspect, RestoreOp)]) -> Vec<&RestoreOp> {
        plan.iter().map(|(_, op)| op).collect()
    }

    #[test]
    fn unchanged_group_restores_source_levels_and_play() {
        let now = household(&[("A", &["A", "B"]), ("C", &["C"])]);
        let RestorePlan { ops: plan, skipped } = plan_restore(
            &snap(&["A", "B"], queue(7), TransportState::Playing),
            &now,
            Some(7),
        );
        assert_eq!(skipped.len(), 0);
        assert_eq!(
            ops_of(&plan),
            [
                &RestoreOp::PlayQueue,
                &RestoreOp::SeekTrack(2),
                &RestoreOp::SeekPosition(40),
                &RestoreOp::SetVolume {
                    player: pid("A"),
                    level: 30
                },
                &RestoreOp::SetMute {
                    player: pid("A"),
                    mute: false
                },
                &RestoreOp::SetVolume {
                    player: pid("B"),
                    level: 30
                },
                &RestoreOp::SetMute {
                    player: pid("B"),
                    mute: false
                },
                &RestoreOp::Play,
            ]
        );
    }

    #[test]
    fn regroups_before_anything_else() {
        // A now plays inside C's group; B went off alone; C was never in A's.
        let now = household(&[("C", &["C", "A"]), ("B", &["B"])]);
        let RestorePlan { ops: plan, .. } = plan_restore(
            &snap(
                &["A", "B"],
                SnapshotSource::Nothing,
                TransportState::Stopped,
            ),
            &now,
            None,
        );
        assert_eq!(
            ops_of(&plan)[..2],
            [
                &RestoreOp::Leave { player: pid("A") },
                &RestoreOp::Join { player: pid("B") }
            ]
        );
        assert_eq!(plan.last().unwrap(), &(Aspect::Transport, RestoreOp::Stop));

        // C joined A's group since: it leaves.
        let now = household(&[("A", &["A", "B", "C"])]);
        let RestorePlan { ops: plan, .. } = plan_restore(
            &snap(
                &["A", "B"],
                SnapshotSource::Nothing,
                TransportState::Stopped,
            ),
            &now,
            None,
        );
        assert_eq!(ops_of(&plan)[0], &RestoreOp::Leave { player: pid("C") });
    }

    #[test]
    fn a_changed_queue_is_not_resumed() {
        let now = household(&[("A", &["A"])]);
        let RestorePlan { ops: plan, skipped } = plan_restore(
            &snap(&["A"], queue(7), TransportState::Playing),
            &now,
            Some(8),
        );
        assert!(
            !plan
                .iter()
                .any(|(a, _)| matches!(a, Aspect::Source | Aspect::Position | Aspect::Transport))
        );
        let aspects: Vec<Aspect> = skipped.iter().map(|(a, _)| *a).collect();
        assert_eq!(
            aspects,
            [Aspect::Source, Aspect::Position, Aspect::Transport]
        );
        assert!(skipped[0].1.contains("queue changed"));
    }

    #[test]
    fn a_stream_seeks_only_with_a_position() {
        let now = household(&[("A", &["A"])]);
        let stream = SnapshotSource::Uri {
            uri: "x-rincon-mp3radio://example.invalid/a.mp3".into(),
            metadata: String::new(),
            position_secs: None,
        };
        let RestorePlan { ops: plan, .. } =
            plan_restore(&snap(&["A"], stream, TransportState::Paused), &now, None);
        assert!(
            !plan
                .iter()
                .any(|(_, op)| matches!(op, RestoreOp::SeekPosition(_)))
        );
        assert!(
            !plan
                .iter()
                .any(|(_, op)| matches!(op, RestoreOp::Play | RestoreOp::Stop))
        );
    }

    #[test]
    fn missing_players_are_reported() {
        let mut now = household(&[("A", &["A"])]);
        now.players.retain(|p| p.id != pid("B"));
        let RestorePlan { ops: plan, skipped } = plan_restore(
            &snap(
                &["A", "B"],
                SnapshotSource::Nothing,
                TransportState::Stopped,
            ),
            &now,
            None,
        );
        assert!(
            skipped
                .iter()
                .any(|(a, why)| *a == Aspect::Group && why.contains('B'))
        );
        assert!(skipped.iter().any(|(a, _)| *a == Aspect::Volume));
        assert!(!plan.iter().any(
            |(_, op)| matches!(op, RestoreOp::SetVolume { player, .. } if *player == pid("B"))
        ));

        now.players.clear();
        let RestorePlan { ops: plan, skipped } = plan_restore(
            &snap(&["A"], SnapshotSource::Nothing, TransportState::Stopped),
            &now,
            None,
        );
        assert_eq!((plan.len(), skipped.len()), (0, 6));
    }
}
