//! Prove the selected occurrence still owned by this DJ. Explicit stops and
//! non-rewinding starts share the same source, queue and group observations.
//! A stop releases a retained session while another source plays, without
//! sending that source a transport command.

use std::net::IpAddr;

use fsonos_proto::content::{BrowseSnapshot, QUEUE, browse_range_snapshot};
use fsonos_proto::soap::{self, AV_TRANSPORT};
use fsonos_proto::{ProtoError, Transport};

use super::recovery::{Group, same_track};
use super::{FeedError, QueueFeed, QueuedWork, Speakers, host_of};

const INSTANCE: &str = "<InstanceID>0</InstanceID>";

/// Live occurrence evidence, never persisted or transferred across a restart.
/// The group includes its boot identity: identical music alone cannot prove
/// that a later read still describes the same running player.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Selected {
    pub(super) position: u32,
    uri: String,
    group: Group,
}

/// A negative observation is not always proof that a different source was
/// selected. A change during the read must never authorize a fresh start.
pub(super) enum Selection {
    Owned(Selected),
    DifferentSource,
    Unowned,
    Changed,
}

fn same_observation(left: &str, right: &str) -> bool {
    !left.is_empty() && (left == right || same_track(left, right))
}

pub(super) fn source<T: Transport + ?Sized>(
    at: &Speakers<'_, T>,
    host: IpAddr,
) -> Result<String, FeedError> {
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
pub(super) fn selected<T: Transport + ?Sized>(
    feed: &QueueFeed,
    at: &Speakers<'_, T>,
) -> Result<Selection, FeedError> {
    let host = host_of(at)?;
    let own_queue = format!("x-rincon-queue:{}#0", at.coordinator.0);
    if source(at, host)? != own_queue {
        return Ok(Selection::DifferentSource);
    }
    let Some(group) = Group::read(at)? else {
        return Ok(Selection::Changed);
    };
    let (position, uri) = occurrence(at, host)?;
    let work = feed.played.iter().chain(&feed.queued).find(|work| {
        work.movements.iter().any(|movement| {
            movement.added
                && movement.position == Some(position)
                && position > 0
                && same_track(&movement.uri, &uri)
        })
    });
    let (first, count) = if let Some(work) = work {
        work.first()
            .filter(|&first| first > 0)
            .zip(work.last())
            .and_then(|(first, last)| {
                last.checked_sub(first)
                    .and_then(|count| count.checked_add(1))
                    .map(|count| (first, count))
            })
            .ok_or_else(|| ProtoError::Malformed("invalid owned DJ queue range".into()))?
    } else {
        // An owner-added track can be a definite non-DJ occurrence, but only
        // after the same bracketing proof. A source switch or seek halfway
        // through this read is not permission to rewind the retained set.
        if position == 0 || uri.is_empty() {
            return Ok(Selection::Changed);
        }
        (position, 1)
    };
    let queue = browse_range_snapshot(at.transport, host, QUEUE, first - 1, count)?;
    let intact = work.map_or_else(
        || {
            queue.objects.len() == 1
                && queue.objects[0]
                    .res
                    .as_ref()
                    .is_some_and(|resource| same_observation(&uri, &resource.uri))
        },
        |work| matches(work, first, &queue),
    );
    if !intact {
        return Ok(Selection::Changed);
    }
    let (selected, selected_uri) = occurrence(at, host)?;
    if selected != position
        || !same_observation(&selected_uri, &uri)
        || source(at, host)? != own_queue
        || Group::read(at)?.as_ref() != Some(&group)
        || browse_range_snapshot(at.transport, host, QUEUE, first - 1, count)? != queue
    {
        return Ok(Selection::Changed);
    }
    // Browse can be slow. A seek, regroup or reboot during its final page
    // need not alter the queue revision, so confirm them after that read too.
    let (selected, selected_uri) = occurrence(at, host)?;
    if selected != position
        || !same_observation(&selected_uri, &uri)
        || Group::read(at)?.as_ref() != Some(&group)
        || source(at, host)? != own_queue
    {
        return Ok(Selection::Changed);
    }
    Ok(work.map_or(Selection::Unowned, |work| {
        Selection::Owned(Selected {
            position,
            uri: work
                .track_at(position)
                .expect("the selected occurrence belongs to this work")
                .to_owned(),
            group,
        })
    }))
}

pub(super) fn owned<T: Transport + ?Sized>(
    feed: &QueueFeed,
    at: &Speakers<'_, T>,
) -> Result<bool, FeedError> {
    Ok(matches!(selected(feed, at)?, Selection::Owned(_)))
}
