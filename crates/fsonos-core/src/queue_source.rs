//! Staged replacement of a queue-backed music source.
//!
//! The existing queue remains intact until the new source has been accepted
//! and its complete insertion verified. Native revision checks protect the
//! subsequent prefix removal against concurrent queue edits. The initial
//! group membership and source remain prerequisites throughout staging;
//! verification failure never starts a replacement on a different group.

use crate::control::queue_removal::{self, Snapshot as Queue, read};
use crate::scenes::queue_backup::Owner;
use crate::{CoreError, HouseholdState, control};
use fsonos_proto::control::{self as soap, Enqueued};
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::{PlayerId, TransportState};
use std::time::Instant;

pub(crate) mod destination;
mod recovery;

use destination::Destination;
pub(crate) use recovery::replace as replace_recovering;

#[derive(Clone, Copy)]
struct Behavior {
    autoplay: bool,
    receipt: bool,
}

/// The exact result of one completed, staged queue replacement. Only the
/// replacement path can issue this receipt; an arbitrary post-action queue
/// read cannot establish which entries the request actually replaced.
pub struct QueueReplacement {
    pub(crate) coordinator: PlayerId,
    pub(crate) before: Queue,
    pub(crate) after: Queue,
    pub(crate) owner: Option<Owner>,
    pub(crate) started: Instant,
    pub(crate) autoplay: bool,
    pub(crate) in_order: bool,
}

impl std::fmt::Debug for QueueReplacement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueueReplacement")
            .field("coordinator", &self.coordinator)
            .field("before_revision", &self.before.revision)
            .field("after_revision", &self.after.revision)
            .field("before_entries", &self.before.uris.len())
            .field("after_entries", &self.after.uris.len())
            .finish_non_exhaustive()
    }
}

/// Undo needs the original playback intent, while the commit must prove the
/// actual execution state after a silent replacement deliberately pauses it.
pub(crate) struct ReceiptOwner {
    original: Owner,
    execution: Option<Owner>,
}

impl ReceiptOwner {
    pub(crate) fn after_pause<T: Transport + ?Sized>(
        mut self,
        t: &T,
        host: std::net::IpAddr,
        coordinator: &PlayerId,
    ) -> Option<Self> {
        self.execution = Some(self.original.after_pause(t, host, coordinator).ok()?);
        Some(self)
    }

    fn confirmed<T: Transport + ?Sized>(
        self,
        t: &T,
        host: std::net::IpAddr,
        coordinator: &PlayerId,
    ) -> Option<Owner> {
        self.execution
            .as_ref()
            .unwrap_or(&self.original)
            .confirm(t, host, coordinator)
            .ok()?;
        Some(self.original)
    }
}

/// Undo eligibility must not turn a successful Play into a playback failure.
/// Missing or inconsistent playback evidence leaves a receipt unusable for
/// undo, while the existing replacement semantics continue unchanged.
pub(crate) fn replacement_owner<T: Transport + ?Sized>(
    t: &T,
    host: std::net::IpAddr,
    coordinator: &PlayerId,
) -> Option<ReceiptOwner> {
    let mode_known = fsonos_proto::play_mode::get_play_mode(t, host).is_ok();
    let owner = Owner::read(t, host, coordinator, mode_known).ok()?;
    owner.confirm(t, host, coordinator).ok()?;
    Some(ReceiptOwner {
        original: owner,
        execution: None,
    })
}

fn appended(before: &Queue, after: &Queue, added: Enqueued) -> bool {
    let Ok(old_len) = u32::try_from(before.uris.len()) else {
        return false;
    };
    let Ok(new_len) = u32::try_from(after.uris.len()) else {
        return false;
    };
    added.count > 0
        && old_len.checked_add(1) == Some(added.first)
        && old_len.checked_add(added.count) == Some(new_len)
        && before.revision != after.revision
        && after.uris.starts_with(&before.uris)
}

/// A check before any write failed: nothing was written.
fn unconfirmed(detail: impl std::fmt::Display) -> CoreError {
    ProtoError::Malformed(format!(
        "queue replacement was not confirmed: {detail}; writes were not replayed; \
         read the queue before trying again"
    ))
    .into()
}

/// A check after a write failed: the replacement may be done, partly done
/// or not done, and nothing was sent again (co55.10).
fn uncertain(detail: impl std::fmt::Display) -> CoreError {
    CoreError::Uncertain(format!(
        "queue replacement was not confirmed: {detail}; writes were not replayed; \
         read the queue before trying again"
    ))
}

/// Play a renderer-ready album or playlist source. The original prefix is
/// retained until an acknowledged, nonempty replacement is read back after
/// it. Lost removal replies are reconciled by reads, never repeated writes.
///
/// This is not an atomic device transaction: a failed append can leave new
/// entries at the end, and a failed start after removal cannot restore the
/// original queue. Errors do not imply that nothing changed. Only an initial
/// enqueue refusal retains its SOAP code for bounded source recovery; once
/// committed, a failed start must not replay the replacement.
pub(crate) fn play<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    uri: &str,
    metadata: &str,
) -> Result<(), CoreError> {
    replace(t, households, coordinator, uri, metadata, true, false).map(|_| ())
}

pub(crate) fn play_with_receipt<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    uri: &str,
    metadata: &str,
) -> Result<QueueReplacement, CoreError> {
    replace(t, households, coordinator, uri, metadata, true, true)
}

/// Replace and select without starting. Silence is established before the
/// append: a playing old queue could otherwise run into the staged tracks.
/// Failures may leave the old source paused or the new tracks appended.
pub(crate) fn load<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    uri: &str,
    metadata: &str,
) -> Result<(), CoreError> {
    replace(t, households, coordinator, uri, metadata, false, false).map(|_| ())
}

pub(crate) fn load_with_receipt<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    uri: &str,
    metadata: &str,
) -> Result<QueueReplacement, CoreError> {
    replace(t, households, coordinator, uri, metadata, false, true)
}

fn replace<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    uri: &str,
    metadata: &str,
    autoplay: bool,
    receipt: bool,
) -> Result<QueueReplacement, CoreError> {
    replace_once(
        t,
        households,
        coordinator,
        uri,
        metadata,
        Behavior { autoplay, receipt },
        None,
    )
}

fn replace_once<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    uri: &str,
    metadata: &str,
    behavior: Behavior,
    mut recovery: Option<&mut recovery::State>,
) -> Result<QueueReplacement, CoreError> {
    let Behavior { autoplay, receipt } = behavior;
    if uri.trim().is_empty() {
        return Err(ProtoError::Malformed("cannot load an empty source URI".into()).into());
    }
    let host = control::locate(households, coordinator)?.ip;
    let boundary = recovery.as_ref().and_then(|state| state.boundary.as_ref());
    let (destination, before) = if let Some(boundary) = boundary {
        boundary.confirm(t, host, coordinator)?;
        (boundary.destination.clone(), boundary.queue.clone())
    } else {
        (
            Destination::read(t, host, coordinator).map_err(unconfirmed)?,
            read(t, host)?,
        )
    };
    destination
        .confirm(t, host, coordinator)
        .map_err(unconfirmed)?;
    let mut owner = if let Some(boundary) = boundary {
        Some(ReceiptOwner {
            original: boundary.owner.clone(),
            execution: None,
        })
    } else {
        (receipt || recovery.is_some())
            .then(|| replacement_owner(t, host, coordinator))
            .flatten()
    };
    if !autoplay {
        // This is not a source-parameter refusal: do not retry the whole
        // replacement when pausing or its read-back fails.
        control::pause_confirmed(t, households, coordinator).map_err(uncertain)?;
        destination
            .confirm(t, host, coordinator)
            .map_err(uncertain)?;
        owner = owner.and_then(|owner| owner.after_pause(t, host, coordinator));
    }
    if let Some(boundary) = boundary {
        // Retain the rejected attempt through all preparation reads. No
        // freshly observed owner or queue may replace it before another add.
        boundary.confirm(t, host, coordinator)?;
    }
    let added = match soap::enqueue_uri(t, host, uri, metadata, 0, false) {
        Ok(added) => added,
        Err(error @ ProtoError::SoapFault { code: 800, .. }) if recovery.is_some() => {
            let proof =
                recovery::after_enqueue_refusal(t, host, coordinator, destination, before, owner)?;
            recovery.as_mut().expect("recovery requested").refused = Some(proof);
            return Err(error.into());
        }
        Err(error @ ProtoError::SoapFault { .. }) => return Err(error.into()),
        Err(error) => return Err(uncertain(error)),
    };
    let staged = read(t, host).map_err(uncertain)?;
    if !appended(&before, &staged, added) {
        return Err(uncertain(
            "the append did not preserve the original prefix and the reported new range",
        ));
    }
    commit_recoverable(
        t,
        households,
        coordinator,
        Staged {
            destination: &destination,
            before: &before,
            after: &staged,
            owner,
        },
        autoplay,
        false,
        recovery.as_ref().and_then(|state| state.boundary.as_ref()),
    )
}

/// A complete replacement already observed after the untouched old prefix.
pub(crate) struct Staged<'a> {
    pub(crate) destination: &'a Destination,
    pub(crate) before: &'a Queue,
    pub(crate) after: &'a Queue,
    pub(crate) owner: Option<ReceiptOwner>,
}

/// Commit either a native container expansion or a known, verified track
/// batch using the same guarded removal and source-selection path.
pub(crate) fn commit<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    staged: Staged<'_>,
    autoplay: bool,
    in_order: bool,
) -> Result<QueueReplacement, CoreError> {
    commit_recoverable(t, households, coordinator, staged, autoplay, in_order, None)
}

fn commit_recoverable<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    staged: Staged<'_>,
    autoplay: bool,
    in_order: bool,
    boundary: Option<&recovery::Recovery>,
) -> Result<QueueReplacement, CoreError> {
    let host = control::locate(households, coordinator)?.ip;
    let Staged {
        destination,
        before,
        after,
        owner,
    } = staged;
    if !autoplay
        && !matches!(
            soap::get_transport_info(t, host).map_err(uncertain)?.state,
            TransportState::Paused | TransportState::Stopped
        )
    {
        return Err(uncertain(
            "playback resumed while the replacement was staged",
        ));
    }
    destination
        .confirm(t, host, coordinator)
        .map_err(uncertain)?;
    let owner = if let Some(boundary) = boundary {
        // Staging the retry may change the queue, but cannot authorize a
        // listener's new playback intent before removing its old prefix.
        boundary
            .owner
            .confirm(t, host, coordinator)
            .map_err(uncertain)?;
        Some(
            owner
                .ok_or_else(|| uncertain("retry preparation lost its playback owner"))?
                .original,
        )
    } else {
        owner.and_then(|owner| owner.confirmed(t, host, coordinator))
    };
    let revision = if before.uris.is_empty() {
        after.revision
    } else {
        if after.revision == 0 {
            return Err(uncertain("the player reports wildcard queue revision zero"));
        }
        let count = u32::try_from(before.uris.len()).map_err(uncertain)?;
        // A refused removal is not a rejected source and cannot authorize
        // another append through render-parameter recovery. The shared
        // verifier also fences coordinator changes and lost acknowledgements.
        queue_removal::remove(t, host, coordinator, after, 1, count)
            .map_err(uncertain)?
            .revision
    };
    // Empty queues skip removal, but not destination verification. Never
    // detach an old coordinator or replace a newly selected source merely
    // because the requested tracks are still visible in its local queue.
    destination
        .confirm(t, host, coordinator)
        .map_err(uncertain)?;
    let receipt = QueueReplacement {
        coordinator: coordinator.clone(),
        before: before.clone(),
        after: Queue {
            revision,
            uris: after.uris[before.uris.len()..].to_vec(),
        },
        owner,
        started: Instant::now(),
        autoplay,
        in_order,
    };
    if !autoplay {
        // The new queue is committed. Even a definite source-selection
        // refusal must not trigger a second append through account recovery.
        return if in_order {
            control::load_queue_from_in_order(t, households, coordinator, 1)
        } else {
            control::load_queue_from(t, households, coordinator, 1)
        }
        .map(|()| receipt)
        .map_err(uncertain);
    }
    let started = if in_order {
        control::play_queue_from_in_order(t, households, coordinator, 1)
    } else {
        control::play_queue_from(t, households, coordinator, 1)
    };
    // The queue has already changed. Even a definite playback refusal must
    // not replay the replacement through render-parameter recovery: a retry
    // could delete a newer queue and lose the original replacement receipt.
    // Lost replies may still be reconciled inside queue_start.
    started.map(|()| receipt).map_err(uncertain)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod ownership;
