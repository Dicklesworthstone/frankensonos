//! Retire played works without treating a queue position as permanent identity.
//!
//! Each removal is revision-addressed and sent once. A lost reply succeeds
//! only when coherent read-back shows exactly the intended removal. Otherwise
//! old cleanup claims are relinquished before resync, so a later identical
//! copy belonging to the owner cannot be adopted and removed instead.

use super::{FeedError, QueueFeed, QueuedWork, Speakers, host_of};
use fsonos_proto::content::{QUEUE, browse_snapshot};
use fsonos_proto::control as soap;
use fsonos_proto::didl::spotify_uri_from_renderer_uri;
use fsonos_proto::{ProtoError, Transport};
use std::net::IpAddr;

#[derive(Debug, PartialEq, Eq)]
struct Queue {
    revision: u32,
    uris: Vec<String>,
}

fn read<T: Transport + ?Sized>(t: &T, host: IpAddr) -> Result<Queue, FeedError> {
    let snapshot = browse_snapshot(t, host, QUEUE)?;
    let uris = snapshot
        .objects
        .into_iter()
        .map(|object| {
            object
                .res
                .map(|resource| resource.uri)
                .filter(|uri| !uri.is_empty())
                .ok_or_else(|| ProtoError::Malformed("queue item has no trim identity".into()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Queue {
        revision: snapshot.update_id,
        uris,
    })
}

/// A changed revision and exactly the intended removal, with every other
/// URI still in order. Length or the target's absence alone is not proof.
fn removed_range(before: &Queue, after: &Queue, start: usize, count: usize) -> bool {
    let Some(end) = start.checked_add(count) else {
        return false;
    };
    count > 0
        && end <= before.uris.len()
        && before.revision != after.revision
        && before.uris.len().checked_sub(count) == Some(after.uris.len())
        && before.uris[..start] == after.uris[..start]
        && before.uris[end..] == after.uris[start..]
}

fn same_track(actual: &str, expected: &str) -> bool {
    actual == expected
        || spotify_uri_from_renderer_uri(actual)
            .as_deref()
            .is_some_and(|uri| uri == expected)
}

/// Only a complete, contiguous work strictly behind the retained boundary.
fn range(work: &QueuedWork, before: u32) -> Option<(u32, u32)> {
    let first = work.movements.first()?.position.filter(|p| *p > 0)?;
    let count = u32::try_from(work.movements.len()).ok()?;
    let last = first.checked_add(count.checked_sub(1)?)?;
    if last >= before {
        return None;
    }
    for (index, movement) in work.movements.iter().enumerate() {
        let position = first.checked_add(u32::try_from(index).ok()?)?;
        if !movement.added || movement.position != Some(position) || movement.uri.is_empty() {
            return None;
        }
    }
    Some((first, count))
}

#[derive(Debug)]
enum Removal {
    Kept,
    Removed(u32),
    Uncertain(ProtoError),
}

fn remove_work<T: Transport + ?Sized>(
    at: &Speakers<'_, T>,
    host: IpAddr,
    work: &QueuedWork,
    before: u32,
    keep_played: u32,
) -> Result<Removal, FeedError> {
    let Some((first, count)) = range(work, before) else {
        return Ok(Removal::Kept);
    };
    let queue = read(at.transport, host)?;
    // Zero disables the speaker's revision comparison. Defer until a later
    // queue edit supplies a usable revision, including after counter wrap.
    if queue.revision == 0 {
        return Ok(Removal::Kept);
    }
    let start = usize::try_from(first - 1)
        .map_err(|_| ProtoError::Malformed("trim position exceeds usize".into()))?;
    let end = start + work.movements.len();
    let Some(found) = queue.uris.get(start..end) else {
        return Ok(Removal::Kept);
    };
    if !found
        .iter()
        .zip(&work.movements)
        .all(|(uri, movement)| same_track(uri, &movement.uri))
    {
        return Ok(Removal::Kept);
    }
    // A stale event can arrive after the owner sought backwards, without
    // changing the queue revision. Check the actual source and track too.
    let position = soap::get_position_info(at.transport, host)?;
    let current = position
        .track
        .checked_sub(1)
        .and_then(|index| usize::try_from(index).ok())
        .and_then(|index| queue.uris.get(index));
    let own_queue = format!("x-rincon-queue:{}#0", at.coordinator.0);
    if range(work, position.track.saturating_sub(keep_played)).is_none()
        || current
            .is_none_or(|uri| !same_track(uri, &position.uri) && !same_track(&position.uri, uri))
        || soap::get_media_info(at.transport, host)?.uri != own_queue
    {
        return Ok(Removal::Kept);
    }
    match soap::remove_track_range_from_queue(at.transport, host, queue.revision, first, count) {
        Ok(()) => Ok(Removal::Removed(count)),
        Err(error @ ProtoError::SoapFault { .. }) => Err(error.into()),
        Err(error) => {
            // A missing response is not permission to repeat a positional
            // delete. Read once more through the bounded snapshot reader.
            if let Ok(after) = read(at.transport, host)
                && removed_range(&queue, &after, start, work.movements.len())
            {
                tracing::warn!(cause = %error, "queue trim confirmed; removal not replayed");
                Ok(Removal::Removed(count))
            } else {
                Ok(Removal::Uncertain(error))
            }
        }
    }
}

impl QueueFeed {
    /// Retire complete old works, back to front, then reconcile positions.
    pub(super) fn trim<T: Transport + ?Sized>(
        &mut self,
        at: &Speakers<'_, T>,
    ) -> Result<(), FeedError> {
        let (Some(playing), Some(length)) = (self.playing, self.queue_len) else {
            return Ok(());
        };
        if length <= self.config.trim_queue_at || self.played.is_empty() {
            return Ok(());
        }
        let host = host_of(at)?;
        let before = playing.saturating_sub(self.config.keep_played);
        let mut removed = 0;
        let mut result = Ok(());
        let mut uncertain = false;
        for index in (0..self.played.len()).rev() {
            match remove_work(
                at,
                host,
                &self.played[index],
                before,
                self.config.keep_played,
            ) {
                Ok(Removal::Kept) => {}
                Ok(Removal::Removed(count)) => {
                    // Forget ownership before the next fallible read or write.
                    self.played.remove(index);
                    removed += count;
                }
                Ok(Removal::Uncertain(error)) => {
                    uncertain = true;
                    self.played.clear();
                    result = Err(ProtoError::Malformed(format!(
                        "queue removal outcome uncertain: {error}; old trim claims dropped, \
                         removal not replayed"
                    ))
                    .into());
                    break;
                }
                Err(error) => {
                    result = Err(error);
                    break;
                }
            }
        }
        if removed > 0 || result.is_err() {
            self.playing = (!uncertain).then_some(playing.saturating_sub(removed));
            // A failed resync must not authorize a partial enqueue using
            // stale positions. The next event retries the read, not removal.
            self.queue_len = None;
            self.resync(at)?;
        }
        result
    }
}

#[cfg(test)]
mod tests;
