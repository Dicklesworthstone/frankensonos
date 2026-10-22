//! Staged replacement of a queue-backed music source.
//!
//! The existing queue remains intact until the new source has been accepted
//! and its complete insertion verified. Native revision checks protect the
//! subsequent prefix removal against concurrent queue edits. The initial
//! group membership and source remain prerequisites throughout staging;
//! verification failure never starts a replacement on a different group.

use crate::control::queue_removal::{self, Snapshot as Queue, read};
use crate::{CoreError, HouseholdState, control};
use fsonos_proto::control::{self as soap, Enqueued};
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::{PlayerId, TransportState};

mod destination;

use destination::Destination;

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

fn unconfirmed(detail: impl std::fmt::Display) -> CoreError {
    ProtoError::Malformed(format!(
        "queue replacement was not confirmed: {detail}; writes were not replayed; \
         read the queue before trying again"
    ))
    .into()
}

/// Play a renderer-ready album or playlist source. The original prefix is
/// retained until an acknowledged, nonempty replacement is read back after
/// it. Lost removal replies are reconciled by reads, never repeated writes.
///
/// This is not an atomic device transaction: a failed append can leave new
/// entries at the end, and a failed start after removal cannot restore the
/// original queue. Errors do not imply that nothing changed. Definite enqueue
/// and playback refusals retain their SOAP codes for bounded source recovery.
pub(crate) fn play<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    uri: &str,
    metadata: &str,
) -> Result<(), CoreError> {
    replace(t, households, coordinator, uri, metadata, true)
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
    replace(t, households, coordinator, uri, metadata, false)
}

fn replace<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    uri: &str,
    metadata: &str,
    autoplay: bool,
) -> Result<(), CoreError> {
    if uri.trim().is_empty() {
        return Err(ProtoError::Malformed("cannot load an empty source URI".into()).into());
    }
    let host = control::locate(households, coordinator)?.ip;
    let destination = Destination::read(t, host, coordinator).map_err(unconfirmed)?;
    let before = read(t, host)?;
    destination
        .confirm(t, host, coordinator)
        .map_err(unconfirmed)?;
    if !autoplay {
        // This is not a source-parameter refusal: do not retry the whole
        // replacement when pausing or its read-back fails.
        control::pause_confirmed(t, households, coordinator).map_err(unconfirmed)?;
        destination
            .confirm(t, host, coordinator)
            .map_err(unconfirmed)?;
    }
    let added = match soap::enqueue_uri(t, host, uri, metadata, 0, false) {
        Ok(added) => added,
        Err(error @ ProtoError::SoapFault { .. }) => return Err(error.into()),
        Err(error) => return Err(unconfirmed(error)),
    };
    let staged = read(t, host).map_err(unconfirmed)?;
    if !appended(&before, &staged, added) {
        return Err(unconfirmed(
            "the append did not preserve the original prefix and the reported new range",
        ));
    }
    if !autoplay
        && !matches!(
            soap::get_transport_info(t, host)
                .map_err(unconfirmed)?
                .state,
            TransportState::Paused | TransportState::Stopped
        )
    {
        return Err(unconfirmed(
            "playback resumed while the replacement was staged",
        ));
    }
    destination
        .confirm(t, host, coordinator)
        .map_err(unconfirmed)?;
    if !before.uris.is_empty() {
        if staged.revision == 0 {
            return Err(unconfirmed(
                "the player reports wildcard queue revision zero",
            ));
        }
        let count = u32::try_from(before.uris.len()).map_err(unconfirmed)?;
        // A refused removal is not a rejected source and cannot authorize
        // another append through render-parameter recovery. The shared
        // verifier also fences coordinator changes and lost acknowledgements.
        queue_removal::remove(t, host, coordinator, &staged, 1, count).map_err(unconfirmed)?;
    }
    // Empty queues skip removal, but not destination verification. Never
    // detach an old coordinator or replace a newly selected source merely
    // because the requested tracks are still visible in its local queue.
    destination
        .confirm(t, host, coordinator)
        .map_err(unconfirmed)?;
    if !autoplay {
        // The new queue is committed. Even a definite source-selection
        // refusal must not trigger a second append through account recovery.
        return control::load_queue_from(t, households, coordinator, 1).map_err(unconfirmed);
    }
    match control::play_queue_from(t, households, coordinator, 1) {
        Ok(()) => Ok(()),
        Err(error @ CoreError::Proto(ProtoError::SoapFault { .. })) => Err(error),
        // The queue has already changed. A lost start reply may be recovered
        // inside queue_start, but must not replay this entire replacement.
        Err(error) => Err(unconfirmed(error)),
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod ownership;
