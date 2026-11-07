//! Reconcile enqueue replies without replaying a queue mutation.
//!
//! One synchronous batch retains a complete queue identity. Each acknowledged
//! single-step mutation can advance it with bounded slot reads; uncertain or
//! irregular observations still require a complete, coherent queue read-back.

use super::FeedError;
use fsonos_proto::content::{BrowseFlag, MAX_BROWSE_OBJECTS, QUEUE, browse, browse_snapshot};
use fsonos_proto::didl::spotify_uri_from_renderer_uri;
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::PlayerId;
use std::net::IpAddr;

mod owner;
mod proof;

use owner::Owner;
use proof::{Expected, Observed, Queue, Slot, observe};

fn invalid(detail: &str) -> FeedError {
    ProtoError::Malformed(detail.into()).into()
}

fn uncertain(cause: Option<&FeedError>, detail: impl std::fmt::Display) -> FeedError {
    let cause = cause.map_or_else(String::new, |error| format!("{error}; "));
    FeedError::UncertainEnqueue(format!("{cause}{detail}; the add was not replayed"))
}

fn identity(uri: String) -> Result<String, FeedError> {
    if uri.trim().is_empty() || uri == "NOT_IMPLEMENTED" {
        return Err(invalid("queue item has no identity for enqueue recovery"));
    }
    Ok(spotify_uri_from_renderer_uri(&uri).unwrap_or(uri))
}

fn read<T: Transport + ?Sized>(transport: &T, host: IpAddr) -> Result<Queue, FeedError> {
    let snapshot = browse_snapshot(transport, host, QUEUE)?;
    let uris = snapshot
        .objects
        .into_iter()
        .map(|object| {
            identity(
                object
                    .res
                    .ok_or_else(|| invalid("queue item has no identity for enqueue recovery"))?
                    .uri,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Queue::new(snapshot.update_id, uris))
}

fn slot<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    offset: u32,
) -> Result<Slot, FeedError> {
    let page = browse(transport, host, QUEUE, BrowseFlag::DirectChildren, offset, 1)?;
    let expected = u32::from(offset < page.total_matches);
    if page.total_matches > MAX_BROWSE_OBJECTS
        || page.number_returned != expected
        || usize::try_from(expected).ok() != Some(page.objects.len())
    {
        return Err(invalid("queue slot returned inconsistent paging counters"));
    }
    let uri = page
        .objects
        .into_iter()
        .next()
        .map(|object| {
            identity(
                object
                    .res
                    .ok_or_else(|| invalid("queue slot has no identity"))?
                    .uri,
            )
        })
        .transpose()?;
    Ok(Slot {
        offset,
        revision: page.update_id,
        total: page.total_matches,
        uri,
    })
}

fn current<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    queue: &Queue,
) -> Result<(), FeedError> {
    if queue.matches_head(&slot(transport, host, 0)?) {
        Ok(())
    } else {
        Err(invalid("the queue changed during enqueue; refresh its positions"))
    }
}

struct ReadBack {
    queue: Queue,
    // A definite refusal is retryable only after owner and final head checks.
    rejection: Option<FeedError>,
}

fn recover<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    before: &Queue,
    expected: Expected,
    uri: &str,
    failure: Option<FeedError>,
) -> Result<ReadBack, FeedError> {
    let after = read(transport, host)
        .map_err(|error| uncertain(failure.as_ref(), format!("queue read-back failed: {error}")))?;
    let observed = observe(before, &after, expected.index, uri);
    if let Some(fault @ FeedError::Proto(ProtoError::SoapFault { .. })) = failure {
        return if observed == Observed::Unchanged {
            Ok(ReadBack {
                queue: after,
                rejection: Some(fault),
            })
        } else {
            Err(uncertain(
                Some(&fault),
                "the queue changed after an explicit refusal",
            ))
        };
    }
    if observed != Observed::Inserted {
        return Err(uncertain(
            failure.as_ref(),
            "queue read-back did not confirm exactly the intended insertion",
        ));
    }
    if let Some(error) = failure {
        tracing::warn!(cause = %error, "enqueue confirmed by read-back; add not replayed");
    }
    Ok(ReadBack {
        queue: after,
        rejection: None,
    })
}

/// Evidence is local to one append/partial-completion call, never serialized,
/// cloned with a feed, or carried across coordinator handoff or restart.
pub(super) struct Batch {
    coordinator: PlayerId,
    owner: Option<Owner>,
    queue: Option<Queue>,
}

impl Batch {
    pub(super) fn new(coordinator: &PlayerId) -> Self {
        Self {
            coordinator: coordinator.clone(),
            owner: None,
            queue: None,
        }
    }

    /// Send at most once. A preflight failure sends nothing; any uncertainty
    /// after the send relinquishes feeding without stopping existing music.
    pub(super) fn confirmed<T: Transport + ?Sized>(
        &mut self,
        transport: &T,
        host: IpAddr,
        uri: &str,
        desired: u32,
        send: impl FnOnce() -> Result<(u32, Option<u32>), FeedError>,
    ) -> Result<(u32, Option<u32>), FeedError> {
        // Taking the cached observation first invalidates it on every error.
        let retained = self.queue.take();
        let observed_owner = Owner::read(transport, host, &self.coordinator)?;
        if self
            .owner
            .as_ref()
            .is_some_and(|owner| owner != &observed_owner)
        {
            return Err(invalid("the DJ queue owner changed before enqueue"));
        }
        self.owner = Some(observed_owner);
        let owner = self.owner.as_ref().expect("owner observed above");
        let mut before = match retained {
            Some(queue) => queue,
            None => {
                let queue = read(transport, host)?;
                owner.confirm(transport, host, &self.coordinator)?;
                queue
            }
        };
        let expected = before
            .expected(desired)
            .filter(|expected| expected.length <= MAX_BROWSE_OBJECTS)
            .filter(|_| !uri.trim().is_empty())
            .ok_or_else(|| {
                invalid("invalid position, empty identity or queue verification limit")
            })?;
        current(transport, host, &before)?;
        let failure = match send() {
            Ok(acknowledgement) if expected.accepts(acknowledgement) => None,
            Ok(_) => Some(invalid(
                "enqueue acknowledgement names a different occurrence or length",
            )),
            Err(error) => Some(error),
        };
        let read_back = if failure.is_none() && owner.reusable() {
            match slot(transport, host, expected.position - 1) {
                Ok(after) if before.proves_single_step(&after, expected, uri) => {
                    before.inserted(expected, uri, after.revision);
                    ReadBack {
                        queue: before,
                        rejection: None,
                    }
                }
                // A failed or irregular slot read is not an add refusal.
                // The complete sequence can still prove the acknowledged add.
                _ => recover(transport, host, &before, expected, uri, None)?,
            }
        } else {
            recover(transport, host, &before, expected, uri, failure)?
        };
        owner
            .confirm(transport, host, &self.coordinator)
            .map_err(|error| uncertain(read_back.rejection.as_ref(), error))?;
        current(transport, host, &read_back.queue)
            .map_err(|error| uncertain(read_back.rejection.as_ref(), error))?;
        if let Some(error) = read_back.rejection {
            return Err(error);
        }
        if owner.reusable() && read_back.queue.cacheable() {
            self.queue = Some(read_back.queue);
        }
        Ok((expected.position, Some(expected.length)))
    }
}

#[cfg(test)]
mod tests;
