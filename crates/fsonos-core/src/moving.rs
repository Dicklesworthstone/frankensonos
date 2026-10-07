//! Moving playback between rooms, and whole-house ("party") mode.
//!
//! `move` takes the music from one room to another as one intent: the target
//! joins the source's group (the move waits, up to [`JOIN_DEADLINE`], for the
//! topology to show it), then the source leaves. When the source leads
//! the group, coordination is first handed to the target with AVTransport
//! `DelegateGroupCoordinationTo` (playback moves with it). A player that does
//! not offer that action (UPnP 401) gets the fallback: the source's playback
//! is replayed on the target (its queue copied, Spotify items re-rendered
//! for the target's household) at the same track and position, and the
//! source stops. Players in different households can never group (S1 and S2
//! are always different households): [`move_playback`] refuses, and
//! [`copy_playback`] replays the music there instead.
//!
//! A DJ session follows its group's coordinator: after a move, re-key it with
//! [`rekey_dj_session`] (or, for a queue the DJ feeds, re-plan on the new
//! coordinator rather than relying on the copied queue).

use crate::rooms::ControlTarget;
use crate::snapshot::{self, SnapshotSource, ZoneSnapshot};
use crate::store::{Store, StoreError};
use crate::{CoreError, HouseholdState, control, grouping, resolve_room};
use fsonos_proto::content;
use fsonos_proto::control as soap;
use fsonos_proto::didl::{
    DidlObject, parse_didl, spotify_queue_uri, spotify_track_didl, spotify_uri_from_renderer_uri,
};
use fsonos_proto::soap::{AV_TRANSPORT, args_xml, call};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::{PlayerId, TransportState};
use std::net::IpAddr;
use std::time::{Duration, Instant};

/// Why a move cannot go ahead as asked.
#[derive(Debug, thiserror::Error)]
pub enum MoveError {
    /// The rooms are in different households, which can never group.
    #[error(
        "{from} and {to} are in different households, which can never be grouped \
         (S1 and S2 are always separate); copy the music there instead"
    )]
    CrossHousehold { from: String, to: String },
    /// The target household has no Spotify favorite to learn its render
    /// parameters from, so Spotify items cannot be replayed there.
    #[error("{0}'s household has no Spotify favorite to learn render parameters from")]
    NoRenderParams(String),
    /// The target never showed up in the source's group.
    #[error("{room} did not join the group within {waited:?}; the music stayed where it was")]
    JoinTimedOut { room: String, waited: Duration },
    #[error(transparent)]
    Core(#[from] CoreError),
}

impl From<ProtoError> for MoveError {
    fn from(e: ProtoError) -> Self {
        Self::Core(e.into())
    }
}

/// One step of a same-household move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveStep {
    /// The target joins the source's group.
    Join,
    /// The source leads the group: hand coordination to the target.
    Delegate,
    /// The source (a member) leaves the group.
    Leave,
}

/// How a move was carried out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveMethod {
    /// Nothing to do: the source and target are the same room.
    Nothing,
    /// The target joined and the source left; the coordinator stayed.
    Regrouped,
    /// The source handed coordination (and playback) to the target.
    Delegated,
    /// The source's playback was replayed on the target and the source
    /// stopped (no delegation on that player, or a cross-household copy).
    Copied,
}

/// What a move did. A DJ session follows `new_coordinator`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveReport {
    pub method: MoveMethod,
    pub old_coordinator: PlayerId,
    pub new_coordinator: PlayerId,
}

/// The steps that move the music from `from` to `to` (same household).
pub fn plan_move(
    from: &ControlTarget<'_>,
    to: &ControlTarget<'_>,
) -> Result<Vec<MoveStep>, MoveError> {
    if !std::ptr::eq(from.household, to.household) {
        return Err(MoveError::CrossHousehold {
            from: from.room.name.clone(),
            to: to.room.name.clone(),
        });
    }
    if from.room.primary == to.room.primary {
        return Ok(Vec::new());
    }
    let mut steps = Vec::new();
    if to.room.coordinator != from.room.coordinator {
        steps.push(MoveStep::Join);
    }
    let source_leads = from.room.players.contains(&from.room.coordinator);
    steps.push(if source_leads {
        MoveStep::Delegate
    } else {
        MoveStep::Leave
    });
    Ok(steps)
}

/// How long a move waits for the target's join to show in the topology.
pub const JOIN_DEADLINE: Duration = Duration::from_secs(5);

/// How often the topology is checked while waiting for a join.
const JOIN_POLL: Duration = Duration::from_millis(100);

/// Move the music from room `from` to room `to` in the same household.
pub fn move_playback<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    from: &ControlTarget<'_>,
    to: &ControlTarget<'_>,
) -> Result<MoveReport, MoveError> {
    move_playback_within(t, households, from, to, JOIN_DEADLINE)
}

/// [`move_playback`], waiting at most `join_deadline` for the target's join
/// to land (players answer the join before the topology shows it). Nothing
/// is handed over or removed until it has.
pub fn move_playback_within<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    from: &ControlTarget<'_>,
    to: &ControlTarget<'_>,
    join_deadline: Duration,
) -> Result<MoveReport, MoveError> {
    let steps = plan_move(from, to)?;
    let coordinator = from.coordinator.id.clone();
    let mut report = MoveReport {
        method: MoveMethod::Nothing,
        old_coordinator: coordinator.clone(),
        new_coordinator: coordinator.clone(),
    };
    for step in steps {
        match step {
            MoveStep::Join => {
                control::join(t, households, &to.player.id, &coordinator)?;
                let host = ip(households, &coordinator)?;
                if !joined_within(t, host, &coordinator, &to.player.id, join_deadline)? {
                    return Err(MoveError::JoinTimedOut {
                        room: to.room.name.clone(),
                        waited: join_deadline,
                    });
                }
                report.method = MoveMethod::Regrouped;
            }
            MoveStep::Leave => {
                control::leave(t, households, &from.player.id)?;
                report.method = MoveMethod::Regrouped;
            }
            MoveStep::Delegate => {
                let host = ip(households, &coordinator)?;
                let args = args_xml(&[
                    ("InstanceID", "0"),
                    ("NewCoordinator", &to.player.id.0),
                    ("RejoinGroup", "0"),
                ]);
                match call(t, host, &AV_TRANSPORT, "DelegateGroupCoordinationTo", &args) {
                    Ok(_) => report.method = MoveMethod::Delegated,
                    Err(ProtoError::SoapFault { code: 401, .. }) => {
                        replay_instead_of_delegating(t, households, &coordinator, &to.player.id)?;
                        report.method = MoveMethod::Copied;
                    }
                    Err(e) => return Err(e.into()),
                }
                report.new_coordinator = to.player.id.clone();
            }
        }
    }
    Ok(report)
}

/// Whether `member` renders in `coordinator`'s group, checking the topology
/// until it does or `deadline` has passed.
fn joined_within<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    coordinator: &PlayerId,
    member: &PlayerId,
    deadline: Duration,
) -> Result<bool, ProtoError> {
    let started = Instant::now();
    loop {
        let joined = get_zone_group_state(t, host)?
            .groups
            .iter()
            .any(|g| g.coordinator == *coordinator && g.members.iter().any(|m| m.uuid == *member));
        let left = deadline.saturating_sub(started.elapsed());
        if joined || left.is_zero() {
            return Ok(joined);
        }
        std::thread::sleep(JOIN_POLL.min(left));
    }
}

/// The fallback for a player without DelegateGroupCoordinationTo: the target
/// (already in the group) leaves, plays what the group played from where it
/// was, takes the group's other rooms along, and the old coordinator stops.
fn replay_instead_of_delegating<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    target: &PlayerId,
) -> Result<(), MoveError> {
    let snap = snapshot::capture(t, households, coordinator, 0)?;
    let queue = queue_of(t, households, &snap)?;
    soap::leave_group(t, ip(households, target)?)?;
    replay_on(t, households, &snap, &queue, target)?;
    for member in snap
        .members
        .iter()
        .filter(|m| *m != coordinator && *m != target)
    {
        soap::join_group(t, ip(households, member)?, target)?;
    }
    soap::stop(t, ip(households, coordinator)?)?;
    Ok(())
}

/// Copy the music `from` plays onto `to`'s group (any household: Spotify
/// items are re-rendered with the target household's parameters), at the
/// same track and position, then stop `from`.
pub fn copy_playback<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    from: &ControlTarget<'_>,
    to: &ControlTarget<'_>,
) -> Result<MoveReport, MoveError> {
    let snap = snapshot::capture(t, households, &from.coordinator.id, 0)?;
    let queue = queue_of(t, households, &snap)?;
    replay_on(t, households, &snap, &queue, &to.coordinator.id)?;
    soap::stop(t, from.coordinator.ip)?;
    Ok(MoveReport {
        method: MoveMethod::Copied,
        old_coordinator: from.coordinator.id.clone(),
        new_coordinator: to.coordinator.id.clone(),
    })
}

/// The queue's items, for a queue source.
fn queue_of<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    snap: &ZoneSnapshot,
) -> Result<Vec<DidlObject>, CoreError> {
    match snap.source {
        SnapshotSource::Queue { .. } => Ok(content::browse_all(
            t,
            ip(households, &snap.coordinator)?,
            content::QUEUE,
        )?),
        _ => Ok(Vec::new()),
    }
}

/// Play `snap`'s source on `target` (which must lead its own group): its
/// URI, or `queue` as the target's new queue, at the same track and
/// position; playing only if it was playing.
fn replay_on<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    snap: &ZoneSnapshot,
    queue: &[DidlObject],
    target: &PlayerId,
) -> Result<(), MoveError> {
    let host = ip(households, target)?;
    let params = || {
        control::spotify_params(t, households, target)?
            .ok_or_else(|| MoveError::NoRenderParams(target.0.clone()))
    };
    let (track, position) = match &snap.source {
        SnapshotSource::Nothing => return Ok(()),
        SnapshotSource::Uri {
            uri,
            metadata,
            position_secs,
        } => {
            if let Some(spotify) = spotify_uri_from_renderer_uri(uri) {
                let title = title_of(metadata);
                let p = params()?;
                soap::set_av_transport_uri(
                    t,
                    host,
                    &fsonos_proto::didl::spotify_track_uri(&spotify, &p),
                    &spotify_track_didl(&spotify, &title, &p),
                )?;
            } else {
                soap::set_av_transport_uri(t, host, uri, metadata)?;
            }
            (0, position_secs.unwrap_or(0))
        }
        SnapshotSource::Queue {
            track,
            position_secs,
            ..
        } => {
            soap::remove_all_tracks_from_queue(t, host)?;
            let mut learned = None;
            for item in queue {
                let Some(res) = &item.res else { continue };
                if let Some(spotify) = spotify_uri_from_renderer_uri(&res.uri) {
                    if learned.is_none() {
                        learned = Some(params()?);
                    }
                    let p = learned.as_ref().expect("learned above");
                    soap::add_uri_to_queue(
                        t,
                        host,
                        &spotify_queue_uri(&spotify),
                        &spotify_track_didl(&spotify, &item.title, p),
                        false,
                    )?;
                } else {
                    soap::add_uri_to_queue(t, host, &res.uri, "", false)?;
                }
            }
            soap::play_from_queue(t, host, target)?;
            (*track, *position_secs)
        }
    };
    if track > 0 {
        soap::seek_track(t, host, track)?;
    }
    if position > 0 {
        soap::seek_position(t, host, position)?;
    }
    if snap.transport_state == TransportState::Playing {
        soap::play(t, host)?;
    }
    Ok(())
}

fn title_of(metadata: &str) -> String {
    parse_didl(metadata)
        .ok()
        .and_then(|objects| objects.into_iter().next())
        .map(|o| o.title)
        .unwrap_or_default()
}

/// Whole-house mode: group every room of `household` under `lead`, or (when
/// `None`) under the group that is playing now, or else its first room.
pub fn party<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    household: &HouseholdState,
    lead: Option<&ControlTarget<'_>>,
) -> Result<grouping::GroupingOutcome, CoreError> {
    let lead = if let Some(l) = lead {
        *l
    } else {
        let playing = household.groups.iter().find(|g| {
            household.player(&g.coordinator).is_some_and(|p| {
                soap::get_transport_info(t, p.ip)
                    .is_ok_and(|info| info.state == TransportState::Playing)
            })
        });
        let id = playing
            .map(|g| g.coordinator.clone())
            .or_else(|| household.rooms.first().map(|r| r.primary.clone()))
            .ok_or_else(|| CoreError::UnknownHousehold("a household with no rooms".into()))?;
        resolve_room(households, &id.0)?
    };
    let rooms = household
        .rooms
        .iter()
        .map(|r| resolve_room(households, &r.primary.0))
        .collect::<Result<Vec<_>, CoreError>>()?;
    Ok(grouping::group(t, households, &lead, &rooms))
}

/// Move the DJ session (if any) from the old coordinator to the new one.
/// Returns whether there was one.
pub fn rekey_dj_session(
    store: &mut dyn Store,
    old: &PlayerId,
    new: &PlayerId,
) -> Result<bool, StoreError> {
    let Some(mut session) = store.dj_session(&old.0)? else {
        return Ok(false);
    };
    store.delete_dj_session(&old.0)?;
    session.coordinator.clone_from(&new.0);
    store.save_dj_session(&session)?;
    Ok(true)
}

fn ip(households: &[HouseholdState], player: &PlayerId) -> Result<IpAddr, CoreError> {
    households
        .iter()
        .find_map(|h| h.player(player))
        .map(|p| p.ip)
        .ok_or_else(|| CoreError::UnknownPlayer(player.0.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Room;
    use crate::store::{DjSession, MemStore};
    use fsonos_types::{Generation, Player, ZoneGroup};

    fn pid(s: &str) -> PlayerId {
        PlayerId(s.into())
    }

    /// Rooms named after their single player, grouped as `groups`.
    fn household(groups: &[(&str, &[&str])]) -> HouseholdState {
        let mut st = HouseholdState::default();
        for (coord, members) in groups {
            st.groups.push(ZoneGroup {
                coordinator: pid(coord),
                members: members.iter().map(|m| pid(m)).collect(),
            });
            for m in *members {
                st.players.push(Player {
                    id: pid(m),
                    room_name: (*m).into(),
                    ip: "192.0.2.1".parse().unwrap(),
                    model: String::new(),
                    generation: Generation::S1,
                });
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

    fn steps(houses: &[HouseholdState], from: &str, to: &str) -> Result<Vec<MoveStep>, MoveError> {
        plan_move(
            &resolve_room(houses, from).unwrap(),
            &resolve_room(houses, to).unwrap(),
        )
    }

    #[test]
    fn planning_covers_every_shape() {
        // A leads a group with B; C plays alone.
        let houses = [household(&[("A", &["A", "B"]), ("C", &["C"])])];
        assert_eq!(
            steps(&houses, "A", "C").unwrap(),
            [MoveStep::Join, MoveStep::Delegate]
        );
        assert_eq!(
            steps(&houses, "A", "B").unwrap(),
            [MoveStep::Delegate],
            "already grouped"
        );
        assert_eq!(
            steps(&houses, "B", "C").unwrap(),
            [MoveStep::Join, MoveStep::Leave]
        );
        assert_eq!(
            steps(&houses, "B", "A").unwrap(),
            [MoveStep::Leave],
            "into its coordinator"
        );
        assert_eq!(steps(&houses, "C", "C").unwrap(), []);

        let mut s2 = household(&[("D", &["D"])]);
        s2.players[0].generation = Generation::S2;
        let houses = [household(&[("A", &["A"])]), s2];
        assert!(matches!(
            steps(&houses, "A", "D"),
            Err(MoveError::CrossHousehold { .. })
        ));
    }

    #[test]
    fn a_dj_session_follows_the_coordinator() {
        let mut store = MemStore::default();
        let session = DjSession {
            coordinator: "RINCON_OLD".into(),
            mood: Some("calm".into()),
            constraints: None,
            expires: 99,
        };
        store.save_dj_session(&session).unwrap();
        assert!(rekey_dj_session(&mut store, &pid("RINCON_OLD"), &pid("RINCON_NEW")).unwrap());
        assert_eq!(store.dj_session("RINCON_OLD").unwrap(), None);
        let moved = store.dj_session("RINCON_NEW").unwrap().unwrap();
        assert_eq!((moved.mood.as_deref(), moved.expires), (Some("calm"), 99));
        assert!(!rekey_dj_session(&mut store, &pid("RINCON_NONE"), &pid("RINCON_X")).unwrap());
    }
}
