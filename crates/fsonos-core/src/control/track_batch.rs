//! Shared destination proof and staged replacement for concrete track batches.
//! Source adapters supply retained native metadata or household-rendered Spotify
//! metadata; insertion and replacement have one ordered mutation contract.

use super::queue_removal::{self, Snapshot};
use super::{QueueReplacement, SourceDestination, addr, pause_confirmed, queue_insert};
use crate::queue_source::{self, Staged};
use crate::{CoreError, HouseholdState};
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::PlayerId;
use std::net::IpAddr;

/// Bound every explicit concrete selection to ten ordered occurrences.
pub const MAX_TRACK_BATCH: usize = 10;

/// The source, group, boot and complete queue observed before source lookup.
/// Retain this proof until insertion begins, so a changed queue cannot become
/// the replacement's new baseline merely because its source URI is unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackBatchDestination {
    pub(crate) source: SourceDestination,
    pub(crate) queue: Snapshot,
}

impl TrackBatchDestination {
    pub fn read<T: Transport + ?Sized>(
        t: &T,
        host: IpAddr,
        coordinator: &PlayerId,
    ) -> Result<Self, ProtoError> {
        let source = SourceDestination::read(t, host, coordinator)?;
        let queue = queue_removal::read(t, host)?;
        let destination = Self { source, queue };
        destination.confirm(t, host, coordinator)?;
        Ok(destination)
    }

    pub fn confirm<T: Transport + ?Sized>(
        &self,
        t: &T,
        host: IpAddr,
        coordinator: &PlayerId,
    ) -> Result<(), ProtoError> {
        self.source.confirm(t, host, coordinator)?;
        if !queue_insert::still_observed(t, host, &self.queue)? {
            return Err(ProtoError::Malformed(
                "the queue changed while preparing selected tracks; nothing was added".into(),
            ));
        }
        Ok(())
    }
}

pub(super) fn replace<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    requests: &[queue_insert::Request<'_>],
    autoplay: bool,
    destination: &TrackBatchDestination,
    receipt: bool,
) -> Result<QueueReplacement, CoreError> {
    let host = addr(households, coordinator)?;
    destination.confirm(t, host, coordinator)?;
    let mut owner = receipt
        .then(|| queue_source::replacement_owner(t, host, coordinator))
        .flatten();
    if !autoplay {
        pause_confirmed(t, households, coordinator)?;
        destination.confirm(t, host, coordinator)?;
        owner = owner.and_then(|owner| owner.after_pause(t, host, coordinator));
    }
    let batch = queue_insert::enqueue_batch(t, host, coordinator, destination, requests, false)?;
    queue_source::commit(
        t,
        households,
        coordinator,
        Staged {
            destination: &destination.source,
            before: &batch.before,
            after: &batch.after,
            owner,
        },
        autoplay,
        true,
    )
    .map_err(|error| {
        CoreError::Uncertain(format!(
            "selected track replacement was not completed: {error}; the batch was not replayed; \
             inspect the queue before retrying"
        ))
    })
}
