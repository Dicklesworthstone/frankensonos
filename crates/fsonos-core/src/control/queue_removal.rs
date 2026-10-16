//! Revision-checked queue deletion shared by direct edits and enqueue undo.
//! One native removal is followed by a complete observation, never a replay.

use crate::CoreError;
use fsonos_proto::content::{MAX_BROWSE_OBJECTS, QUEUE, browse_snapshot};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport, control as soap};
use fsonos_types::PlayerId;
use std::net::IpAddr;
use std::ops::Range;

/// Every URI in one coherent queue revision, in queue order.
#[derive(Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub revision: u32,
    pub uris: Vec<String>,
}

/// The observed result, not a predicted queue length.
#[derive(Debug, PartialEq, Eq)]
pub struct Removed {
    pub remaining: u32,
    pub recovered: bool,
}

/// Distinguish refusal before a write from an uncertain result after one.
#[derive(Debug, thiserror::Error)]
pub enum RemovalError {
    #[error("{0}")]
    Invalid(&'static str),
    #[error("{0}")]
    Before(#[source] ProtoError),
    #[error("{0}")]
    Refused(#[source] ProtoError),
    #[error("queue removal outcome is uncertain: {0}; the write was not replayed")]
    Uncertain(String),
}

impl From<RemovalError> for CoreError {
    fn from(error: RemovalError) -> Self {
        match error {
            RemovalError::Before(cause) | RemovalError::Refused(cause) => cause.into(),
            other => ProtoError::Malformed(other.to_string()).into(),
        }
    }
}

fn identity(uri: &str) -> bool {
    !uri.trim().is_empty() && uri != "NOT_IMPLEMENTED"
}

/// Read the entire queue using the protocol layer's bounded, coherent browse.
pub fn read<T: Transport + ?Sized>(t: &T, host: IpAddr) -> Result<Snapshot, ProtoError> {
    let snapshot = browse_snapshot(t, host, QUEUE)?;
    let uris = snapshot
        .objects
        .into_iter()
        .map(|item| {
            item.res
                .map(|res| res.uri)
                .filter(|uri| identity(uri))
                .ok_or_else(|| {
                    ProtoError::Malformed(
                        "queue item has no identity for removal verification".into(),
                    )
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Snapshot {
        revision: snapshot.update_id,
        uris,
    })
}

fn range(first: u32, count: u32) -> Result<Range<usize>, RemovalError> {
    let start = first.checked_sub(1);
    let end = start.and_then(|start| start.checked_add(count));
    match (start, end) {
        (Some(start), Some(end)) if count > 0 && count <= MAX_BROWSE_OBJECTS => {
            let start = usize::try_from(start)
                .map_err(|_| RemovalError::Invalid("queue position is too large"))?;
            let end = usize::try_from(end)
                .map_err(|_| RemovalError::Invalid("queue position is too large"))?;
            Ok(start..end)
        }
        _ => Err(RemovalError::Invalid("invalid queue removal range")),
    }
}

/// Exact prefix/suffix preservation, including duplicate URIs and counter wrap.
#[must_use]
pub fn removed(before: &Snapshot, after: &Snapshot, start: usize, end: usize) -> bool {
    start < end
        && end <= before.uris.len()
        && before.revision != after.revision
        && before.uris.len() - (end - start) == after.uris.len()
        && before.uris[..start] == after.uris[..start]
        && before.uris[end..] == after.uris[start..]
}

fn coordinator<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    expected: &PlayerId,
) -> Result<bool, ProtoError> {
    Ok(get_zone_group_state(t, host)?
        .groups
        .iter()
        .any(|group| group.coordinator == *expected))
}

fn uncertain(write: Option<&ProtoError>, detail: impl std::fmt::Display) -> RemovalError {
    let detail = match write {
        Some(cause) => format!("{detail} (write reply: {cause})"),
        None => detail.to_string(),
    };
    RemovalError::Uncertain(detail)
}

/// Delete a range from `before` exactly once. The caller selects identities
/// from that snapshot; the device's nonzero UpdateID closes the browse/write
/// race. Check the original coordinator before and after, never redirect the
/// removal to another queue. Explicit SOAP refusals cannot be inferred away.
pub fn remove<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    expected: &PlayerId,
    before: &Snapshot,
    first: u32,
    count: u32,
) -> Result<Removed, RemovalError> {
    let selected = range(first, count)?;
    if selected.end > before.uris.len()
        || before.uris.len() > usize::try_from(MAX_BROWSE_OBJECTS).unwrap_or(usize::MAX)
        || before.uris.iter().any(|uri| !identity(uri))
    {
        return Err(RemovalError::Invalid(
            "the queue snapshot or selection is incomplete",
        ));
    }
    // Zero is a wildcard, including when a real counter has wrapped to zero.
    if before.revision == 0 {
        return Err(RemovalError::Invalid(
            "the player reports wildcard queue revision zero; nothing was removed",
        ));
    }
    if !coordinator(t, host, expected).map_err(RemovalError::Before)? {
        return Err(RemovalError::Invalid(
            "the queue coordinator changed; nothing was removed",
        ));
    }
    let write_error =
        match soap::remove_track_range_from_queue(t, host, before.revision, first, count) {
            Ok(()) => None,
            Err(cause @ (ProtoError::Network { .. } | ProtoError::Malformed(_))) => Some(cause),
            Err(cause) => return Err(RemovalError::Refused(cause)),
        };
    let after = read(t, host).map_err(|error| uncertain(write_error.as_ref(), error))?;
    if !removed(before, &after, selected.start, selected.end) {
        return Err(uncertain(
            write_error.as_ref(),
            "read-back did not confirm exactly the selected deletion",
        ));
    }
    if !coordinator(t, host, expected).map_err(|error| uncertain(write_error.as_ref(), error))? {
        return Err(uncertain(
            write_error.as_ref(),
            "the queue coordinator changed during removal",
        ));
    }
    Ok(Removed {
        remaining: u32::try_from(after.uris.len())
            .map_err(|_| uncertain(write_error.as_ref(), "the resulting queue is too large"))?,
        recovered: write_error.is_some(),
    })
}

/// Undo only the recorded consecutive range, not matching songs elsewhere.
pub(super) fn undo<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    expected: &PlayerId,
    first: u32,
    uris: &[String],
) -> Result<bool, RemovalError> {
    let count = u32::try_from(uris.len())
        .map_err(|_| RemovalError::Invalid("queued range exceeds u32 tracks"))?;
    let selected = range(first, count)?;
    if uris.iter().any(|uri| !identity(uri)) {
        return Err(RemovalError::Invalid(
            "queued track identity is empty or unavailable",
        ));
    }
    let before = read(t, host).map_err(RemovalError::Before)?;
    if before.uris.get(selected) != Some(uris) {
        return Ok(false);
    }
    remove(t, host, expected, &before, first, count).map(|_| true)
}

#[cfg(test)]
mod tests;
