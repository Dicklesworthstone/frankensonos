//! Staged replacement of a queue-backed music source.
//!
//! The existing queue remains intact until the new source has been accepted
//! and its complete insertion verified. Native revision checks protect the
//! subsequent prefix removal against concurrent queue edits.

use crate::{CoreError, HouseholdState, control};
use fsonos_proto::content::{QUEUE, browse_snapshot};
use fsonos_proto::control::{self as soap, Enqueued};
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::PlayerId;
use std::net::IpAddr;

#[derive(Debug, PartialEq, Eq)]
struct Queue {
    revision: u32,
    uris: Vec<String>,
}

fn read<T: Transport + ?Sized>(t: &T, host: IpAddr) -> Result<Queue, ProtoError> {
    let snapshot = browse_snapshot(t, host, QUEUE)?;
    let uris = snapshot
        .objects
        .into_iter()
        .map(|item| {
            item.res
                .map(|res| res.uri)
                .filter(|uri| !uri.trim().is_empty() && uri != "NOT_IMPLEMENTED")
                .ok_or_else(|| {
                    ProtoError::Malformed("queue entry has no verifiable identity".into())
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Queue {
        revision: snapshot.update_id,
        uris,
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
    let host = control::locate(households, coordinator)?.ip;
    let before = read(t, host)?;
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
    if !before.uris.is_empty() {
        if staged.revision == 0 {
            return Err(unconfirmed(
                "the player reports wildcard queue revision zero",
            ));
        }
        let count = u32::try_from(before.uris.len()).map_err(unconfirmed)?;
        let removed = soap::remove_track_range_from_queue(t, host, staged.revision, 1, count);
        let remaining = read(t, host).map_err(unconfirmed)?;
        if remaining.revision == staged.revision
            || remaining.uris.as_slice() != &staged.uris[before.uris.len()..]
        {
            // A removal's 800 is not a rejected source. Never let it trigger
            // another append through the caller's render-parameter retry.
            return Err(unconfirmed(removed.err().map_or_else(
                || "the old prefix removal did not match read-back".to_string(),
                |error| error.to_string(),
            )));
        }
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
