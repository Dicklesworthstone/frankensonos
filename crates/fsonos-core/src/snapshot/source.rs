//! Restore a source without replaying uncertain writes or the whole snapshot.
//! The one transient-402 retry stays inside the source step, before Seek or
//! Play can run. A changed source, group or queue during settling cancels it.

use super::{RestoreOp, SnapshotSource, ZoneSnapshot, queue_update_id, run};
use crate::{CoreError, HouseholdState};
use fsonos_proto::control::{self as soap, MediaInfo};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::{PlayerId, TransportState};
use std::net::IpAddr;
use std::time::Duration;

/// A just-finished announcement can briefly refuse a source with UPnP 402.
const SETTLE: Duration = Duration::from_millis(750);

struct Target {
    uri: String,
    metadata: String,
    revision: Option<u32>,
}

impl Target {
    fn of(snap: &ZoneSnapshot, op: &RestoreOp) -> Option<Self> {
        match op {
            RestoreOp::PlayQueue => Some(Self {
                uri: format!("x-rincon-queue:{}#0", snap.coordinator.0),
                metadata: String::new(),
                revision: match snap.source {
                    SnapshotSource::Queue { update_id, .. } => Some(update_id),
                    _ => None,
                },
            }),
            RestoreOp::SetUri { uri, metadata } => Some(Self {
                uri: uri.clone(),
                metadata: metadata.clone(),
                revision: None,
            }),
            _ => None,
        }
    }

    fn matches(&self, media: &MediaInfo) -> bool {
        media.uri == self.uri
            && (self.metadata.trim().is_empty()
                || media.uri_metadata.trim() == self.metadata.trim())
    }

    fn queue_unchanged<T: Transport + ?Sized>(&self, t: &T, host: IpAddr) -> Result<(), CoreError> {
        if let Some(expected) = self.revision
            && queue_update_id(t, host)? != expected
        {
            return Err(unproven("the queue changed during snapshot restoration"));
        }
        Ok(())
    }

    fn confirmed<T: Transport + ?Sized>(
        &self,
        t: &T,
        host: IpAddr,
        coordinator: &PlayerId,
        members: &[PlayerId],
    ) -> Result<bool, CoreError> {
        if !self.matches(&soap::get_media_info(t, host)?) {
            return Ok(false);
        }
        self.queue_unchanged(t, host)?;
        if group(t, host, coordinator)?.as_slice() != members {
            return Ok(false);
        }
        self.queue_unchanged(t, host)?;
        // A source switch during the preceding reads must not authorize Seek.
        Ok(self.matches(&soap::get_media_info(t, host)?))
    }
}

#[derive(PartialEq, Eq)]
struct Anchor {
    media: MediaInfo,
    track: u32,
    track_uri: String,
    state: TransportState,
    members: Vec<PlayerId>,
}

impl Anchor {
    fn read<T: Transport + ?Sized>(
        t: &T,
        host: IpAddr,
        coordinator: &PlayerId,
    ) -> Result<Self, CoreError> {
        let media = soap::get_media_info(t, host)?;
        let position = soap::get_position_info(t, host)?;
        Ok(Self {
            media,
            track: position.track,
            track_uri: position.uri,
            state: soap::get_transport_info(t, host)?.state,
            members: group(t, host, coordinator)?,
        })
    }
}

fn group<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    coordinator: &PlayerId,
) -> Result<Vec<PlayerId>, CoreError> {
    let topology = get_zone_group_state(t, host)?;
    let group = topology
        .groups
        .iter()
        .find(|g| g.coordinator == *coordinator)
        .ok_or_else(|| unproven("the snapshot player no longer leads its group"))?;
    let mut members: Vec<_> = group.members.iter().map(|m| m.uuid.clone()).collect();
    members.sort_by_key(|p| p.0.clone());
    Ok(members)
}

fn unproven(why: &str) -> CoreError {
    ProtoError::Malformed(why.into()).into()
}

pub(super) fn restore<T: Transport + ?Sized>(
    t: &T,
    household: &HouseholdState,
    host: IpAddr,
    snap: &ZoneSnapshot,
    op: &RestoreOp,
    vet: &dyn Fn(&RestoreOp) -> Result<RestoreOp, String>,
) -> Result<(), CoreError> {
    let Some(target) = Target::of(snap, op) else {
        return run(t, household, host, &snap.coordinator, op);
    };
    let before = Anchor::read(t, host, &snap.coordinator)?;
    target.queue_unchanged(t, host)?;
    let first = run(t, household, host, &snap.coordinator, op);
    let result = match first {
        Err(error @ CoreError::Proto(ProtoError::SoapFault { code: 402, .. })) => {
            std::thread::sleep(SETTLE);
            // This is a definite refusal, not an indeterminate write. Still,
            // retry only against the context the first attempt observed.
            let unchanged = vet(op).is_ok_and(|approved| approved == *op)
                && target.queue_unchanged(t, host).is_ok()
                && Anchor::read(t, host, &snap.coordinator).is_ok_and(|after| after == before);
            if !unchanged {
                return Err(error);
            }
            run(t, household, host, &snap.coordinator, op)
        }
        result => result,
    };
    match result {
        Ok(()) => {
            if target.confirmed(t, host, &snap.coordinator, &before.members)? {
                Ok(())
            } else {
                Err(unproven(
                    "the snapshot source was not confirmed after its write",
                ))
            }
        }
        Err(error) => {
            let ambiguous = matches!(
                &error,
                CoreError::Proto(ProtoError::Network { .. } | ProtoError::Malformed(_))
                    | CoreError::Uncertain(_)
            );
            if ambiguous
                && target
                    .confirmed(t, host, &snap.coordinator, &before.members)
                    .unwrap_or(false)
            {
                // Read-back proves the effect. Do not reset this source again.
                Ok(())
            } else {
                Err(error)
            }
        }
    }?;
    if matches!(op, RestoreOp::PlayQueue)
        && let Some(mode) = snap.play_mode
    {
        // Ordering is part of restoring this queue source. Recheck the same
        // source authorization before its setting, and do not resume unless
        // both the setting and the original queue/group still agree.
        if !vet(op).is_ok_and(|approved| approved == *op) {
            return Err(unproven("queue restoration authorization changed"));
        }
        super::play_mode::restore(t, host, &snap.coordinator, mode)?;
        if !target.confirmed(t, host, &snap.coordinator, &before.members)? {
            return Err(unproven("the queue changed while restoring its play mode"));
        }
    }
    Ok(())
}
