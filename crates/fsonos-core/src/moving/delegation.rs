//! Confirm a native handoff after any reply without repeating it. Only an
//! acknowledged no-op or a definite refusal on the unchanged group may use
//! replay; a request with an unknown outcome must never clear another queue.

use super::{JOIN_POLL, MoveError};
use fsonos_proto::control::{self, MediaInfo, PositionInfo, TransportInfo};
use fsonos_proto::soap::{self, AV_TRANSPORT};
use fsonos_proto::topology::{ZoneGroupInfo, ZoneGroupState, get_zone_group_state};
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::{PlayerId, TransportState};
use std::collections::BTreeSet;
use std::net::IpAddr;
use std::time::{Duration, Instant};

type Members = BTreeSet<String>;

fn members(group: &ZoneGroupInfo) -> Members {
    group
        .members
        .iter()
        .flat_map(|m| std::iter::once(m).chain(&m.satellites))
        .map(|m| m.uuid.0.clone())
        .collect()
}

fn group<'a>(state: &'a ZoneGroupState, leader: &PlayerId) -> Option<&'a ZoneGroupInfo> {
    let mut groups = state.groups.iter().filter(|g| g.coordinator == *leader);
    let found = groups.next()?;
    groups.next().is_none().then_some(found)
}

pub(super) struct Layout {
    source: PlayerId,
    target: PlayerId,
    original: Members,
    retained: Members,
    moved: Members,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Observed {
    Unchanged,
    Transferred,
    Changed,
}

impl Layout {
    pub(super) fn capture(
        state: &ZoneGroupState,
        source: &PlayerId,
        target: &PlayerId,
    ) -> Result<Self, MoveError> {
        crate::topology::validate(state)?;
        let original_group = group(state, source).ok_or_else(changed)?;
        let original = members(original_group);
        let leader = original_group
            .members
            .iter()
            .find(|m| m.uuid == *source)
            .ok_or_else(changed)?;
        // A source room takes its bonded players with it, not the other rooms.
        // An offline bonded player absent from the topology adds no evidence.
        let retained: Members = leader
            .bonded_players()
            .filter(|p| original.contains(&p.0))
            .map(|p| p.0.clone())
            .collect();
        let moved: Members = original.difference(&retained).cloned().collect();
        if !retained.contains(&source.0) || !moved.contains(&target.0) {
            return Err(changed());
        }
        Ok(Self {
            source: source.clone(),
            target: target.clone(),
            original,
            retained,
            moved,
        })
    }

    fn observe(&self, state: &ZoneGroupState) -> Observed {
        if crate::topology::validate(state).is_err() {
            return Observed::Changed;
        }
        let source = group(state, &self.source).map(members);
        let target = group(state, &self.target).map(members);
        if source.as_ref() == Some(&self.retained) && target.as_ref() == Some(&self.moved) {
            Observed::Transferred
        } else if source.as_ref() == Some(&self.original) && target.is_none() {
            Observed::Unchanged
        } else {
            Observed::Changed
        }
    }

    pub(super) fn require_unchanged(&self, state: &ZoneGroupState) -> Result<(), MoveError> {
        if self.observe(state) != Observed::Unchanged {
            return Err(changed());
        }
        Ok(())
    }

    /// Copied playback must carry every original room, including its bonded
    /// players, before the old coordinator can be stopped. A join reply can
    /// precede the topology change or acknowledge a no-op; only reads repeat.
    pub(super) fn confirm_replay<T: Transport + ?Sized>(
        &self,
        t: &T,
        source_host: IpAddr,
        target_host: IpAddr,
        deadline: Duration,
    ) -> Result<(), MoveError> {
        let started = Instant::now();
        loop {
            let observed = read(t, source_host, target_host)?;
            if self.observe(&observed) == Observed::Transferred {
                return Ok(());
            }
            let remaining = deadline.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(MoveError::SourceChanged(
                    "not every room reached the copied playback group; the original source was not stopped"
                        .into(),
                ));
            }
            std::thread::sleep(JOIN_POLL.min(remaining));
        }
    }
}

fn changed() -> MoveError {
    MoveError::SourceChanged(
        "the handoff no longer matches the prepared group or source; replay was not attempted"
            .into(),
    )
}

/// The destination is the useful witness after handoff: the old coordinator
/// may stop answering. A failed destination read may still use the old one.
fn read<T: Transport + ?Sized>(
    t: &T,
    source_host: IpAddr,
    target_host: IpAddr,
) -> Result<ZoneGroupState, ProtoError> {
    get_zone_group_state(t, target_host).or_else(|_| get_zone_group_state(t, source_host))
}

/// A queue URI changes coordinator during handoff, but its selected occurrence
/// and clock travel with it. Live streams need no track clock.
struct Playback {
    media: MediaInfo,
    transport: TransportInfo,
    selection: Option<PositionInfo>,
    observed: Instant,
}

impl Playback {
    fn read<T: Transport + ?Sized>(
        t: &T,
        host: IpAddr,
        leader: &PlayerId,
    ) -> Result<Self, MoveError> {
        let observed = Instant::now();
        let media = control::get_media_info(t, host)?;
        let transport = control::get_transport_info(t, host)?;
        let empty_stopped = media.tracks == 0 && transport.state == TransportState::Stopped;
        let selection = if !empty_stopped && media.uri == format!("x-rincon-queue:{}#0", leader.0) {
            Some(control::get_position_info(t, host)?)
        } else {
            None
        };
        if !crate::snapshot::same_media(&media, &control::get_media_info(t, host)?) {
            return Err(changed());
        }
        Ok(Self {
            media,
            transport,
            selection,
            observed,
        })
    }

    fn matches_source(&self, now: &Self, from: &PlayerId, to: &PlayerId) -> bool {
        let mut expected = self.media.clone();
        if expected.uri == format!("x-rincon-queue:{}#0", from.0) {
            expected.uri = format!("x-rincon-queue:{}#0", to.0);
        }
        if !crate::snapshot::same_media(&expected, &now.media) {
            return false;
        }
        match (&self.selection, &now.selection) {
            (None, None) => true,
            (Some(before), Some(after)) => {
                let forward = if self.transport.state == TransportState::Playing {
                    u32::try_from(self.observed.elapsed().as_secs())
                        .unwrap_or(u32::MAX)
                        .saturating_add(1)
                } else {
                    0
                };
                before.track == after.track
                    && before.uri == after.uri
                    && match (before.position_secs, after.position_secs) {
                        (Some(start), Some(end)) => {
                            (start..=start.saturating_add(forward)).contains(&end)
                        }
                        (None, None) => true,
                        _ => false,
                    }
            }
            _ => false,
        }
    }

    fn known_intent(&self) -> bool {
        match self.transport.state {
            TransportState::Playing | TransportState::Paused => {
                !matches!(self.media.uri.trim(), "" | "NOT_IMPLEMENTED")
            }
            TransportState::Stopped => true,
            TransportState::Unknown => self.media.uri.is_empty(),
            TransportState::Transitioning => false,
        }
    }

    fn known_selection(&self) -> bool {
        let Some(position) = &self.selection else {
            return true;
        };
        if position.track == 0 {
            return self.transport.state == TransportState::Stopped && position.uri.is_empty();
        }
        position.track <= self.media.tracks
            && !matches!(position.uri.trim(), "" | "NOT_IMPLEMENTED")
    }
}

fn incomplete(detail: impl std::fmt::Display) -> MoveError {
    crate::CoreError::Uncertain(format!(
        "group coordination transferred but playback continuity could not be confirmed: {detail}; inspect the destination before another move"
    )).into()
}

/// Once the group has transferred, no subsequent observation can authorize a
/// fallback copy. A transitioning renderer may settle within the original wait.
fn confirm_transfer<T: Transport + ?Sized>(
    t: &T,
    source_host: IpAddr,
    target_host: IpAddr,
    layout: &Layout,
    before: &Playback,
    started: Instant,
    deadline: Duration,
) -> Result<(), MoveError> {
    loop {
        let now = Playback::read(t, target_host, &layout.target).map_err(incomplete)?;
        if !now.known_selection() || !before.matches_source(&now, &layout.source, &layout.target) {
            return Err(incomplete(
                "the source, selected queue occurrence or position changed",
            ));
        }
        let topology = read(t, source_host, target_host).map_err(incomplete)?;
        if layout.observe(&topology) != Observed::Transferred {
            return Err(incomplete("the transferred group changed"));
        }
        if now.transport.state == before.transport.state && now.transport.status != "ERROR_OCCURRED"
        {
            return Ok(());
        }
        let remaining = deadline.saturating_sub(started.elapsed());
        if now.transport.state != TransportState::Transitioning
            || before.transport.state != TransportState::Playing
            || now.transport.status == "ERROR_OCCURRED"
            || remaining.is_zero()
        {
            return Err(incomplete(format!(
                "expected {:?}, observed {:?} with status {:?}",
                before.transport.state, now.transport.state, now.transport.status
            )));
        }
        std::thread::sleep(JOIN_POLL.min(remaining));
    }
}

/// `true`: verified native transfer. `false`: replay is permitted against the
/// still-unchanged source. A lost reply returns an error unless transfer can be
/// observed; even an unchanged read cannot prove that request stopped running.
pub(super) fn handoff<T: Transport + ?Sized>(
    t: &T,
    source_host: IpAddr,
    target_host: IpAddr,
    source: &PlayerId,
    target: &PlayerId,
    deadline: Duration,
) -> Result<bool, MoveError> {
    let layout = Layout::capture(&get_zone_group_state(t, source_host)?, source, target)?;
    let before = Playback::read(t, source_host, source)?;
    if !before.known_intent() || !before.known_selection() {
        return Err(MoveError::SourceChanged(
            "the original playback or selected queue occurrence could not be confirmed before handoff"
                .into(),
        ));
    }
    if layout.observe(&get_zone_group_state(t, source_host)?) != Observed::Unchanged {
        return Err(changed());
    }
    let started = Instant::now();
    let sent = soap::call(
        t,
        source_host,
        &AV_TRANSPORT,
        "DelegateGroupCoordinationTo",
        &soap::args_xml(&[
            ("InstanceID", "0"),
            ("NewCoordinator", &target.0),
            ("RejoinGroup", "0"),
        ]),
    )
    .map(drop);
    let refused = matches!(&sent, Err(ProtoError::SoapFault { .. }));
    let may_replay = sent.is_ok() || refused;
    loop {
        let state = read(t, source_host, target_host);
        let remaining = deadline.saturating_sub(started.elapsed());
        match state {
            Ok(state) => match layout.observe(&state) {
                Observed::Transferred => {
                    confirm_transfer(
                        t,
                        source_host,
                        target_host,
                        &layout,
                        &before,
                        started,
                        deadline,
                    )?;
                    return Ok(true);
                }
                Observed::Unchanged if refused || remaining.is_zero() => {
                    if !may_replay {
                        return sent.map(|()| false).map_err(MoveError::from);
                    }
                    // Refusal alone does not authorize copying a newer source.
                    let current = Playback::read(t, source_host, source)?;
                    if !before.matches_source(&current, source, source)
                        || current.transport.state != before.transport.state
                        || !current.known_selection()
                        || current.transport.status == "ERROR_OCCURRED"
                        || layout.observe(&read(t, source_host, target_host)?)
                            != Observed::Unchanged
                    {
                        return Err(changed());
                    }
                    return Ok(false);
                }
                Observed::Changed if remaining.is_zero() => return Err(changed()),
                _ => {}
            },
            Err(error) if remaining.is_zero() => {
                return Err(sent.err().unwrap_or(error).into());
            }
            Err(_) => {}
        }
        std::thread::sleep(JOIN_POLL.min(remaining));
    }
}
