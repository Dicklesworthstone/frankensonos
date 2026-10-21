//! A buffered stop is not permission to restart a queue the owner has left.

use std::collections::HashSet;

use fsonos_core::playback::PlayerPlayback;
use fsonos_proto::Transport;
use fsonos_proto::content::{BrowseFlag, BrowseSnapshot, QUEUE, browse, browse_snapshot};
use fsonos_proto::control as soap;
use fsonos_proto::didl::spotify_uri_from_renderer_uri;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_types::TransportState;

use super::{FeedError, QueueFeed, Speakers, host_of};

/// Only the existing dry-queue/retry predicate adds observations. Evaluate it
/// on a provisional feed: neither failed reads nor stale notifications consume
/// the recovery cursor, record a play, or discard the owner's retained works.
pub(super) fn permits<T: Transport + ?Sized>(
    feed: &QueueFeed,
    at: &Speakers<'_, T>,
    event: &PlayerPlayback,
) -> Result<bool, FeedError> {
    // Ordinary pushed state must not clone the whole retained queue either.
    if event.transport != Some(TransportState::Stopped)
        || (feed.dry_end.is_none() && !feed.ran_dry(event))
    {
        return Ok(true);
    }
    let Some((end, _)) = feed.clone().dry(event) else {
        return Ok(true);
    };
    if feed.coordinator() != *at.coordinator {
        return Ok(false);
    }
    let host = host_of(at)?;
    let own_queue = format!("x-rincon-queue:{}#0", at.coordinator.0);
    let topology = get_zone_group_state(at.transport, host)?;
    if !topology
        .groups
        .iter()
        .any(|group| group.coordinator == *at.coordinator)
        || soap::get_media_info(at.transport, host)?.uri != own_queue
        || soap::get_transport_info(at.transport, host)?.state != TransportState::Stopped
    {
        return Ok(false);
    }
    let queue = browse_snapshot(at.transport, host, QUEUE)?;
    if !owned_end(feed, &queue, end) {
        return Ok(false);
    }
    let position = soap::get_position_info(at.transport, host)?;
    let current = position
        .track
        .checked_sub(1)
        .and_then(|index| usize::try_from(index).ok())
        .and_then(|index| queue.objects.get(index))
        .and_then(|object| object.res.as_ref());
    if current.is_none_or(|resource| !same_track(&resource.uri, &position.uri))
        || event.queue_position != Some(position.track)
        || !event
            .track_uri
            .as_deref()
            .is_some_and(|uri| same_track(uri, &position.uri))
    {
        return Ok(false);
    }
    // A read can race a pause, seek, source switch or coordinator handoff.
    // Recheck around the observation; never send Play to obtain the evidence.
    let topology = get_zone_group_state(at.transport, host)?;
    if !topology
        .groups
        .iter()
        .any(|group| group.coordinator == *at.coordinator)
        || soap::get_media_info(at.transport, host)?.uri != own_queue
    {
        return Ok(false);
    }
    let revision = browse(at.transport, host, QUEUE, BrowseFlag::DirectChildren, 0, 1)?;
    if revision.update_id != queue.update_id
        || usize::try_from(revision.total_matches).ok() != Some(queue.objects.len())
        || revision.number_returned != 1
        || revision.objects.len() != 1
        || revision.objects.first() != queue.objects.first()
    {
        return Ok(false);
    }
    let after = soap::get_position_info(at.transport, host)?;
    Ok(after.track == position.track
        && same_track(&after.uri, &position.uri)
        && soap::get_media_info(at.transport, host)?.uri == own_queue
        && soap::get_transport_info(at.transport, host)?.state == TransportState::Stopped)
}

/// Preserve exact occurrences. Reassigning an old claim to an identical later
/// copy would let an owner edit silently become permission to restart playback.
fn owned_end(feed: &QueueFeed, queue: &BrowseSnapshot, end: u32) -> bool {
    if u32::try_from(queue.objects.len()).ok() != feed.left_at() {
        return false;
    }
    let mut positions = HashSet::new();
    for movement in feed
        .played
        .iter()
        .chain(&feed.queued)
        .flat_map(|work| &work.movements)
        .filter(|movement| movement.added && movement.position.is_some())
    {
        let position = movement.position.expect("filtered positioned movement");
        if !positions.insert(position) {
            return false;
        }
        let resource = position
            .checked_sub(1)
            .and_then(|index| usize::try_from(index).ok())
            .and_then(|index| queue.objects.get(index))
            .and_then(|object| object.res.as_ref());
        if resource.is_none_or(|resource| !same_track(&resource.uri, &movement.uri)) {
            return false;
        }
    }
    // After a refused Play, partial top-up events can lag behind additions.
    // Accept those only when every item beyond the original end is our own.
    positions.contains(&end)
        && queue.objects.iter().enumerate().all(|(index, _)| {
            u32::try_from(index + 1)
                .is_ok_and(|position| position <= end || positions.contains(&position))
        })
}

fn same_track(left: &str, right: &str) -> bool {
    if left.is_empty() || right.is_empty() {
        return false;
    }
    let left_source = spotify_uri_from_renderer_uri(left);
    let right_source = spotify_uri_from_renderer_uri(right);
    left_source.as_deref().unwrap_or(left) == right_source.as_deref().unwrap_or(right)
}

#[cfg(test)]
mod tests;
