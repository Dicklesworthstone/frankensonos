//! Insert a known bounded track list as one verified consecutive range.
//! The native protocol has no batch transaction; a partial result is never
//! replayed or mistaken for a refused, unchanged request.

use super::{Plan, Queued, Request, Snapshot, inserted, invalid, prepare, read, same_track};
use super::{still_observed, uncertain};
use crate::CoreError;
use crate::control::{SourceDestination, TrackBatchDestination};
use fsonos_proto::content::MAX_BROWSE_OBJECTS;
use fsonos_proto::control::{self as soap, Enqueued, PositionInfo};
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::PlayerId;
use std::net::IpAddr;

pub(in super::super) struct Batch {
    pub(in super::super) before: Snapshot,
    pub(in super::super) after: Snapshot,
    pub(in super::super) queued: Queued,
}

fn guard<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    coordinator: &PlayerId,
    destination: &SourceDestination,
    queue: &Snapshot,
    position: Option<&PositionInfo>,
) -> Result<(), ProtoError> {
    destination.confirm(t, host, coordinator)?;
    if !still_observed(t, host, queue)? {
        return Err(ProtoError::Malformed("the batch queue changed".into()));
    }
    if let Some(before) = position {
        let now = soap::get_position_info(t, host)?;
        if now.track != before.track || !same_track(&now.uri, &before.uri) {
            return Err(ProtoError::Malformed(
                "the playing occurrence changed during next-track insertion".into(),
            ));
        }
    }
    Ok(())
}

fn receipt<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    before: &Snapshot,
    after: &Snapshot,
    result: Result<Enqueued, ProtoError>,
    expected_first: u32,
    previous: usize,
) -> Result<(Enqueued, Option<ProtoError>), CoreError> {
    match result {
        Ok(added) => Ok((added, None)),
        Err(error @ ProtoError::SoapFault { .. }) => {
            if previous == 0 && before == after && still_observed(t, host, after)? {
                return Err(error.into());
            }
            Err(uncertain(
                Some(&error),
                "the track batch was not completely inserted",
            ))
        }
        Err(error @ (ProtoError::Network { .. } | ProtoError::Malformed(_))) => Ok((
            Enqueued {
                first: expected_first,
                count: 1,
            },
            Some(error),
        )),
        Err(error) => Err(uncertain(Some(&error), "the track was not acknowledged")),
    }
}

fn prepare_batch<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    coordinator: &PlayerId,
    destination: &TrackBatchDestination,
    count: usize,
    next: bool,
) -> Result<Plan, CoreError> {
    if count == 0 || count > crate::control::MAX_SPOTIFY_TRACK_BATCH {
        return Err(invalid(
            "a Spotify track batch must contain between 1 and 10 tracks",
        ));
    }
    destination.confirm(t, host, coordinator)?;
    let plan = prepare(t, host, coordinator, next)?;
    if plan.before != destination.queue {
        return Err(invalid(
            "the queue changed while resolving Spotify tracks; nothing was added",
        ));
    }
    if plan.before.uris.len().saturating_add(count)
        > usize::try_from(MAX_BROWSE_OBJECTS).unwrap_or(usize::MAX)
    {
        return Err(invalid(
            "the batch exceeds the bounded queue verification limit",
        ));
    }
    Ok(plan)
}

pub(in super::super) fn enqueue_batch<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    coordinator: &PlayerId,
    destination: &TrackBatchDestination,
    requests: &[Request<'_>],
    next: bool,
) -> Result<Batch, CoreError> {
    let Plan {
        before,
        playing_queue,
        as_next,
        first,
        position,
        ..
    } = prepare_batch(t, host, coordinator, destination, requests.len(), next)?;
    let total = u32::try_from(requests.len()).map_err(|_| invalid("track batch is too large"))?;
    let destination = &destination.source;
    let mut after = before.clone();
    let mut undo_revision_chain = true;
    for written in 0..requests.len() {
        guard(t, host, coordinator, destination, &after, position.as_ref()).map_err(|error| {
            match written {
                0 => error.into(),
                _ => uncertain(None, error),
            }
        })?;
        // Every EnqueueAsNext inserts after the same current occurrence. Add
        // the list backwards so both native next flags and queue order agree.
        let index = if as_next {
            requests.len() - 1 - written
        } else {
            written
        };
        let request = &requests[index];
        let expected_first = if as_next {
            first
        } else {
            first + u32::try_from(written).expect("bounded track batch")
        };
        let result = soap::enqueue_uri(
            t,
            host,
            request.uri,
            request.metadata,
            if as_next { first } else { 0 },
            as_next,
        );
        let observed = read(t, host).map_err(|error| uncertain(result.as_ref().err(), error))?;
        undo_revision_chain &= after.revision.checked_add(1) == Some(observed.revision);
        destination
            .confirm(t, host, coordinator)
            .map_err(|error| uncertain(result.as_ref().err(), error))?;
        let (added, lost) = receipt(t, host, &after, &observed, result, expected_first, written)?;
        inserted(&after, &observed, added.first, added.count)
            .filter(|selected| {
                added.first == expected_first
                    && selected.len() == 1
                    && same_track(&selected[0], request.source_uri)
            })
            .ok_or_else(|| {
                uncertain(
                    lost.as_ref(),
                    "read-back did not prove the exact batch insertion",
                )
            })?;
        guard(
            t,
            host,
            coordinator,
            destination,
            &observed,
            position.as_ref(),
        )
        .map_err(|error| uncertain(lost.as_ref(), error))?;
        if let Some(error) = lost {
            tracing::warn!(cause = %error, "batch track confirmed without replaying its add");
        }
        after = observed;
    }
    let selected = inserted(&before, &after, first, total)
        .filter(|selected| {
            selected
                .iter()
                .zip(requests)
                .all(|(uri, request)| same_track(uri, request.source_uri))
        })
        .ok_or_else(|| uncertain(None, "the complete ordered track batch was not confirmed"))?;
    let queued = Queued {
        first,
        uris: selected.to_vec(),
        playing_queue,
        proof: undo_revision_chain.then(|| {
            crate::control::queue_removal::QueuedProof::confirmed(
                host, coordinator, destination, &before, &after, first, total, total,
            )
        }).flatten(),
    };
    Ok(Batch {
        before,
        after,
        queued,
    })
}
