//! Confirm a native handoff after any reply without repeating it. Only an
//! acknowledged no-op or a definite refusal on the unchanged group may use
//! replay; a request with an unknown outcome must never clear another queue.

use super::{JOIN_POLL, MoveError};
use fsonos_proto::control::{self, MediaInfo};
use fsonos_proto::soap::{self, AV_TRANSPORT};
use fsonos_proto::topology::{ZoneGroupInfo, ZoneGroupState, get_zone_group_state};
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::PlayerId;
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

struct Layout {
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
    fn capture(
        state: &ZoneGroupState,
        source: &PlayerId,
        target: &PlayerId,
    ) -> Result<Self, MoveError> {
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

fn transferred_media(before: &MediaInfo, after: &MediaInfo, layout: &Layout) -> bool {
    let old_queue = format!("x-rincon-queue:{}#0", layout.source.0);
    let expected = if before.uri == old_queue {
        format!("x-rincon-queue:{}#0", layout.target.0)
    } else {
        before.uri.clone()
    };
    after.uri == expected && after.tracks == before.tracks
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
    let before = control::get_media_info(t, source_host)?;
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
                    let media = control::get_media_info(t, target_host)?;
                    if !transferred_media(&before, &media, &layout)
                        || layout.observe(&read(t, source_host, target_host)?)
                            != Observed::Transferred
                    {
                        return Err(changed());
                    }
                    return Ok(true);
                }
                Observed::Unchanged if refused || remaining.is_zero() => {
                    if !may_replay {
                        return sent.map(|()| false).map_err(MoveError::from);
                    }
                    // Refusal alone does not authorize copying a newer source.
                    if control::get_media_info(t, source_host)? != before
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
