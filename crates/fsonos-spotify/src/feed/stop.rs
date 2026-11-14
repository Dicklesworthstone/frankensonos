//! Stop only the selected occurrence still owned by this DJ. An explicit stop
//! also releases a retained session while an announcement or another source
//! plays, without sending that source a transport command.

use std::net::IpAddr;

use fsonos_proto::content::{BrowseSnapshot, QUEUE, browse_range_snapshot};
use fsonos_proto::soap::{self, AV_TRANSPORT};
use fsonos_proto::{ProtoError, Transport};

use super::recovery::{Group, same_track};
use super::{FeedError, QueueFeed, QueuedWork, Speakers, host_of};

const INSTANCE: &str = "<InstanceID>0</InstanceID>";

fn source<T: Transport + ?Sized>(at: &Speakers<'_, T>, host: IpAddr) -> Result<String, FeedError> {
    let response = soap::call(at.transport, host, &AV_TRANSPORT, "GetMediaInfo", INSTANCE)?;
    // A missing field is an unreadable prerequisite, not proof that the
    // listener selected another source.
    Ok(response.require("CurrentURI")?.to_owned())
}

fn occurrence<T: Transport + ?Sized>(
    at: &Speakers<'_, T>,
    host: IpAddr,
) -> Result<(u32, String), FeedError> {
    let response = soap::call(
        at.transport,
        host,
        &AV_TRANSPORT,
        "GetPositionInfo",
        INSTANCE,
    )?;
    Ok((
        response.require_u32("Track")?,
        response.require("TrackURI")?.to_owned(),
    ))
}

fn matches(work: &QueuedWork, first: u32, queue: &BrowseSnapshot) -> bool {
    work.movements.iter().all(|movement| {
        let Some(position) = movement.position else {
            return true;
        };
        movement.added
            && position
                .checked_sub(first)
                .and_then(|index| usize::try_from(index).ok())
                .and_then(|index| queue.objects.get(index))
                .and_then(|object| object.res.as_ref())
                .is_some_and(|resource| same_track(&movement.uri, &resource.uri))
    })
}

/// Check fresh source, group incarnation and the held work's exact queue
/// positions. Do not resync a stale claim onto another copy of the same music.
/// These observations precede Stop; the device offers no atomic ownership lock.
pub(super) fn owned<T: Transport + ?Sized>(
    feed: &QueueFeed,
    at: &Speakers<'_, T>,
) -> Result<bool, FeedError> {
    let host = host_of(at)?;
    let own_queue = format!("x-rincon-queue:{}#0", at.coordinator.0);
    if source(at, host)? != own_queue {
        return Ok(false);
    }
    let Some(group) = Group::read(at)? else {
        return Ok(false);
    };
    let (position, uri) = occurrence(at, host)?;
    let Some(work) = feed.played.iter().chain(&feed.queued).find(|work| {
        work.movements.iter().any(|movement| {
            movement.added
                && movement.position == Some(position)
                && position > 0
                && same_track(&movement.uri, &uri)
        })
    }) else {
        return Ok(false);
    };
    let first = work.first().filter(|&first| first > 0);
    let range = first.zip(work.last()).and_then(|(first, last)| {
        last.checked_sub(first)
            .and_then(|count| count.checked_add(1))
            .map(|count| (first, count))
    });
    let (first, count) =
        range.ok_or_else(|| ProtoError::Malformed("invalid owned DJ queue range".into()))?;
    let queue = browse_range_snapshot(at.transport, host, QUEUE, first - 1, count)?;
    if !matches(work, first, &queue) {
        return Ok(false);
    }
    let (selected, selected_uri) = occurrence(at, host)?;
    if selected != position
        || !same_track(&selected_uri, &uri)
        || source(at, host)? != own_queue
        || Group::read(at)?.as_ref() != Some(&group)
        || browse_range_snapshot(at.transport, host, QUEUE, first - 1, count)? != queue
    {
        return Ok(false);
    }
    // Browse can be slow. A seek, regroup or reboot during its final page
    // need not alter the queue revision, so confirm them after that read too.
    let (selected, selected_uri) = occurrence(at, host)?;
    Ok(selected == position
        && same_track(&selected_uri, &uri)
        && Group::read(at)?.as_ref() == Some(&group)
        && source(at, host)? == own_queue)
}
