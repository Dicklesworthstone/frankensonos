//! Observe one enqueue without replaying an uncertain AddURIToQueue.
//!
//! Full bounded snapshots are deliberate: a matching URI or queue length alone
//! cannot distinguish an insertion from an old copy or an unrelated edit.

use super::Queued;
use super::queue_removal::{Snapshot, read};
use crate::CoreError;
use fsonos_proto::content::{BrowseFlag, MAX_BROWSE_OBJECTS, QUEUE, browse};
use fsonos_proto::control::{self as soap, Enqueued};
use fsonos_proto::didl::{SpotifyContainer, spotify_uri_from_renderer_uri};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::PlayerId;
use std::net::IpAddr;

pub(super) struct Request<'a> {
    pub source_uri: &'a str,
    pub uri: &'a str,
    pub metadata: &'a str,
    pub next: bool,
}

fn invalid(detail: &str) -> CoreError {
    ProtoError::Malformed(detail.into()).into()
}

fn uncertain(cause: Option<&ProtoError>, detail: impl std::fmt::Display) -> CoreError {
    let cause = cause.map_or_else(String::new, |error| format!(" (write reply: {error})"));
    invalid(&format!(
        "queue insertion outcome is uncertain: {detail}{cause}; the add was not replayed; \
         inspect the queue before trying again"
    ))
}

fn leads<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    expected: &PlayerId,
) -> Result<bool, ProtoError> {
    Ok(get_zone_group_state(t, host)?
        .groups
        .iter()
        .any(|group| group.coordinator == *expected))
}

fn same_track(left: &str, right: &str) -> bool {
    let left_source = spotify_uri_from_renderer_uri(left);
    let right_source = spotify_uri_from_renderer_uri(right);
    left_source.as_deref().unwrap_or(left) == right_source.as_deref().unwrap_or(right)
}

/// Preserve exact occurrences, including repeated URIs on either side.
fn inserted<'a>(
    before: &Snapshot,
    after: &'a Snapshot,
    first: u32,
    count: u32,
) -> Option<&'a [String]> {
    let start = usize::try_from(first.checked_sub(1)?).ok()?;
    let count = usize::try_from(count).ok()?;
    let end = start.checked_add(count)?;
    if count == 0
        || start > before.uris.len()
        || before.revision == after.revision
        || before.uris.len().checked_add(count) != Some(after.uris.len())
        || before.uris[..start] != after.uris[..start]
        || before.uris[start..] != after.uris[end..]
    {
        return None;
    }
    after.uris.get(start..end)
}

fn still_observed<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    queue: &Snapshot,
) -> Result<bool, ProtoError> {
    let head = browse(t, host, QUEUE, BrowseFlag::DirectChildren, 0, 1)?;
    Ok(head.update_id == queue.revision
        && usize::try_from(head.total_matches).ok() == Some(queue.uris.len())
        && usize::try_from(head.number_returned).ok() == Some(queue.uris.len().min(1))
        && head.objects.len() == queue.uris.len().min(1)
        && head
            .objects
            .first()
            .and_then(|object| object.res.as_ref())
            .map(|res| &res.uri)
            == queue.uris.first())
}

struct Plan {
    before: Snapshot,
    source: String,
    playing_queue: bool,
    as_next: bool,
    first: u32,
}

fn prepare<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    coordinator: &PlayerId,
    next: bool,
) -> Result<Plan, CoreError> {
    if !leads(t, host, coordinator)? {
        return Err(invalid("the enqueue target no longer coordinates its queue"));
    }
    let source = soap::get_media_info(t, host)?.uri;
    let playing_queue = source == format!("x-rincon-queue:{}#0", coordinator.0);
    let before = read(t, host)?;
    let length =
        u32::try_from(before.uris.len()).map_err(|_| invalid("the queue length exceeds u32"))?;
    if length >= MAX_BROWSE_OBJECTS {
        return Err(invalid(
            "the queue reached the bounded enqueue verification limit",
        ));
    }
    let as_next = next && playing_queue && length > 0;
    let position = if as_next {
        let current = soap::get_position_info(t, host)?;
        let index = current
            .track
            .checked_sub(1)
            .and_then(|index| usize::try_from(index).ok());
        if index
            .and_then(|index| before.uris.get(index))
            .is_none_or(|uri| !same_track(uri, &current.uri))
        {
            return Err(invalid("the playing queue occurrence could not be verified"));
        }
        Some(current)
    } else {
        None
    };
    let first = position
        .as_ref()
        .map_or(length + 1, |current| current.track + 1);
    // A source switch or queue edit during preflight cannot select another slot.
    if soap::get_media_info(t, host)?.uri != source
        || !leads(t, host, coordinator)?
        || !still_observed(t, host, &before)?
    {
        return Err(invalid(
            "queue or source changed before enqueue; nothing was added",
        ));
    }
    if let Some(current) = &position {
        let now = soap::get_position_info(t, host)?;
        if now.track != current.track || !same_track(&now.uri, &current.uri) {
            return Err(invalid("the current queue track changed before enqueue"));
        }
    }

    Ok(Plan {
        before,
        source,
        playing_queue,
        as_next,
        first,
    })
}

/// Acknowledged containers require the reported range and unchanged neighbors.
/// Lost container replies remain uncertain: the unknown expansion cannot be
/// inferred from an arbitrary new suffix. A known single track can be recovered.
pub(super) fn enqueue<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    coordinator: &PlayerId,
    request: &Request<'_>,
) -> Result<Queued, CoreError> {
    let expected = if SpotifyContainer::of(request.source_uri).is_some() {
        None
    } else if request
        .source_uri
        .strip_prefix("spotify:track:")
        .is_some_and(|id| !id.is_empty())
    {
        Some(request.source_uri)
    } else {
        return Err(invalid("enqueue requires a Spotify track, album or playlist"));
    };
    let Plan {
        before,
        source,
        playing_queue,
        as_next,
        first,
    } = prepare(t, host, coordinator, request.next)?;

    let written = soap::enqueue_uri(
        t,
        host,
        request.uri,
        request.metadata,
        if as_next { first } else { 0 },
        as_next,
    );
    let after = read(t, host).map_err(|error| uncertain(written.as_ref().err(), error))?;
    let same_source = soap::get_media_info(t, host)
        .map_err(|error| uncertain(written.as_ref().err(), error))?
        .uri
        == source;
    if !same_source
        || !leads(t, host, coordinator)
            .map_err(|error| uncertain(written.as_ref().err(), error))?
    {
        return Err(uncertain(
            written.as_ref().err(),
            "the original queue coordinator or source changed",
        ));
    }
    let (added, lost) = match written {
        Err(cause @ ProtoError::SoapFault { .. }) => {
            // Only an unchanged rejected attempt can authorize account relearning.
            // Never infer success from a definite refusal, even if an edit matches.
            return if before == after
                && still_observed(t, host, &after)
                    .map_err(|error| uncertain(Some(&cause), error))?
            {
                Err(cause.into())
            } else {
                Err(uncertain(Some(&cause), "the queue changed after a refusal"))
            };
        }
        Ok(added) => (added, None),
        Err(cause @ (ProtoError::Network { .. } | ProtoError::Malformed(_)))
            if expected.is_some() =>
        {
            (Enqueued { first, count: 1 }, Some(cause))
        }
        Err(cause) => {
            return Err(uncertain(
                Some(&cause),
                "no acknowledged track range for this source",
            ));
        }
    };
    let selected = inserted(&before, &after, added.first, added.count)
        .filter(|_| added.first == first)
        .ok_or_else(|| uncertain(lost.as_ref(), "read-back did not show exactly the insertion"))?;
    if expected.is_some_and(|uri| selected.len() != 1 || !same_track(&selected[0], uri)) {
        return Err(uncertain(
            lost.as_ref(),
            "the inserted track is not the requested track",
        ));
    }
    if !still_observed(t, host, &after).map_err(|error| uncertain(lost.as_ref(), error))? {
        return Err(uncertain(lost.as_ref(), "the queue changed during read-back"));
    }
    if let Some(cause) = lost {
        tracing::warn!(cause = %cause, "enqueue confirmed by read-back; add not replayed");
    }
    Ok(Queued {
        first: added.first,
        uris: selected.to_vec(),
        playing_queue,
    })
}

#[cfg(test)]
mod tests;
