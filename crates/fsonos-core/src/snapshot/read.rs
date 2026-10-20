//! Read-only playback capture. Bracket room reads with source, queue, track,
//! mode and transport observations; do not combine different playback epochs.

use super::{MemberLevel, SnapshotSource, ZoneSnapshot, ip, member_rooms};
use crate::{CoreError, HouseholdState};
use fsonos_proto::content::{self, BrowseFlag};
use fsonos_proto::control::{self as soap, MediaInfo, PositionInfo};
use fsonos_proto::didl::spotify_uri_from_renderer_uri;
use fsonos_proto::play_mode::get_play_mode;
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::{PlayMode, PlayerId, TransportState};
use std::net::IpAddr;
use std::time::{Duration, Instant};

mod group;

const ATTEMPTS: usize = 3;

#[derive(Clone, Copy, PartialEq, Eq)]
struct QueueStamp {
    revision: u32,
    tracks: u32,
}

struct Playback {
    media: MediaInfo,
    state: TransportState,
    position: PositionInfo,
    queue: Option<QueueStamp>,
    mode: Option<PlayMode>,
}

fn changed() -> CoreError {
    ProtoError::Malformed("playback changed while capturing the snapshot; retry the action".into())
        .into()
}

fn same_track(a: &str, b: &str) -> bool {
    a == b
        || spotify_uri_from_renderer_uri(a).is_some_and(|uri| {
            spotify_uri_from_renderer_uri(b).as_deref() == Some(uri.as_str())
        })
}

fn queue_stamp<T: Transport + ?Sized>(t: &T, host: IpAddr) -> Result<QueueStamp, CoreError> {
    let page = content::browse(t, host, content::QUEUE, BrowseFlag::DirectChildren, 0, 1)?;
    Ok(QueueStamp {
        revision: page.update_id,
        tracks: page.total_matches,
    })
}

/// Read the revision and selected entry together, so a position cannot name
/// a different queue item even when the device's reads lag one another.
fn selected<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    position: &PositionInfo,
) -> Result<(QueueStamp, bool), CoreError> {
    let page = content::browse(
        t,
        host,
        content::QUEUE,
        BrowseFlag::DirectChildren,
        position.track.saturating_sub(1),
        1,
    )?;
    let matches = if position.track == 0 {
        position.uri.is_empty()
    } else {
        position.track <= page.total_matches
            && page.number_returned == 1
            && page.objects.len() == 1
            && page.objects[0]
                .res
                .as_ref()
                .is_some_and(|res| same_track(&res.uri, &position.uri))
    };
    Ok((
        QueueStamp {
            revision: page.update_id,
            tracks: page.total_matches,
        },
        matches,
    ))
}

impl Playback {
    fn begin<T: Transport + ?Sized>(
        t: &T,
        host: IpAddr,
        coordinator: &PlayerId,
    ) -> Result<Self, CoreError> {
        let media = soap::get_media_info(t, host)?;
        let own_queue = format!("x-rincon-queue:{}#0", coordinator.0);
        if media.uri.starts_with("x-rincon:")
            || (media.uri.starts_with("x-rincon-queue:") && media.uri != own_queue)
        {
            // Never restore another coordinator's queue as this player's own.
            return Err(changed());
        }
        let state = soap::get_transport_info(t, host)?.state;
        let queue = (media.uri == own_queue)
            .then(|| queue_stamp(t, host))
            .transpose()?;
        let mode = queue.and_then(|_| get_play_mode(t, host).ok());
        let position = soap::get_position_info(t, host)?;
        Ok(Self {
            media,
            state,
            position,
            queue,
            mode,
        })
    }

    fn finish<T: Transport + ?Sized>(
        &self,
        t: &T,
        host: IpAddr,
    ) -> Result<(Self, bool), CoreError> {
        // Reverse the opening read order. Source/state/revision changes during
        // a slow room read invalidate the entire candidate, including levels.
        let position = soap::get_position_info(t, host)?;
        let mode = self.queue.and_then(|_| get_play_mode(t, host).ok());
        let (queue, selection_matches) = match self.queue {
            Some(_) => {
                let (stamp, matches) = selected(t, host, &position)?;
                (Some(stamp), matches)
            }
            None => (None, true),
        };
        let state = soap::get_transport_info(t, host)?.state;
        let media = soap::get_media_info(t, host)?;
        Ok((
            Self {
                media,
                state,
                position,
                queue,
                mode,
            },
            selection_matches,
        ))
    }

    fn agrees(&self, after: &Self, elapsed: Duration) -> bool {
        self.media == after.media
            && self.state == after.state
            && matches!(
                self.state,
                TransportState::Playing | TransportState::Paused | TransportState::Stopped
            )
            && self.queue == after.queue
            && self.mode == after.mode
            && self.position.track == after.position.track
            && same_track(&self.position.uri, &after.position.uri)
            && self.position.duration_secs == after.position.duration_secs
            // Browse is authoritative for queue length. GetMediaInfo can omit
            // NrTracks (the protocol reader represents it as zero).
            && self.queue.is_none_or(|q| {
                after.position.track <= q.tracks
                    && (after.state != TransportState::Playing || after.position.track > 0)
            })
            && progresses(
                self.position.position_secs,
                after.position.position_secs,
                self.state,
                elapsed,
            )
    }

    fn source(self) -> SnapshotSource {
        if let Some(queue) = self.queue {
            SnapshotSource::Queue {
                track: self.position.track,
                position_secs: self.position.position_secs.unwrap_or(0),
                update_id: queue.revision,
            }
        } else if self.media.uri.is_empty() {
            SnapshotSource::Nothing
        } else {
            SnapshotSource::Uri {
                uri: self.media.uri,
                metadata: self.media.uri_metadata,
                position_secs: self
                    .position
                    .position_secs
                    .filter(|_| self.position.duration_secs.is_some_and(|d| d > 0)),
            }
        }
    }
}

fn progresses(
    before: Option<u32>,
    after: Option<u32>,
    state: TransportState,
    elapsed: Duration,
) -> bool {
    match (before, after) {
        (None, None) => true,
        (Some(before), Some(after)) if state == TransportState::Playing => {
            // Positions have whole-second precision. One boundary crossing is
            // possible even for a very short capture, but a seek is not progress.
            let advance = u32::try_from(elapsed.as_secs())
                .unwrap_or(u32::MAX)
                .saturating_add(1);
            after >= before && after <= before.saturating_add(advance)
        }
        _ => before == after,
    }
}

pub(super) fn capture<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    captured_at: i64,
) -> Result<ZoneSnapshot, CoreError> {
    let household = households
        .iter()
        .find(|h| h.player(coordinator).is_some())
        .ok_or_else(|| CoreError::UnknownPlayer(coordinator.0.clone()))?;
    for _ in 0..ATTEMPTS {
        let (fresh, group) = group::Group::begin(t, household, coordinator)?;
        let household = &fresh;
        let host = ip(household, coordinator)?;
        let started = Instant::now();
        let before = Playback::begin(t, host, coordinator)?;
        let members = member_rooms(household, coordinator);
        let levels = members
            .iter()
            .map(|player| {
                let at = ip(household, player)?;
                Ok(MemberLevel {
                    player: player.clone(),
                    volume: soap::get_volume(t, at)?,
                    mute: soap::get_mute(t, at)?,
                })
            })
            .collect::<Result<Vec<_>, CoreError>>()?;
        let group_volume = soap::get_group_volume(t, host).ok();
        let group_unchanged = group.unchanged(t, host)?;
        let (after, selection_matches) = before.finish(t, host)?;
        if group_unchanged && selection_matches && before.agrees(&after, started.elapsed()) {
            return Ok(ZoneSnapshot {
                household: household.id.clone(),
                coordinator: coordinator.clone(),
                members,
                levels,
                group_volume,
                transport_state: after.state,
                play_mode: after.mode,
                source: after.source(),
                captured_at,
            });
        }
    }
    Err(changed())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_progress_is_not_an_owner_seek() {
        let playing = TransportState::Playing;
        assert!(progresses(Some(10), Some(11), playing, Duration::ZERO));
        assert!(progresses(Some(10), Some(14), playing, Duration::from_secs(3)));
        assert!(!progresses(Some(10), Some(15), playing, Duration::from_secs(3)));
        assert!(!progresses(Some(10), Some(9), playing, Duration::from_secs(3)));
        assert!(progresses(Some(u32::MAX), Some(u32::MAX), playing, Duration::MAX));
    }

    #[test]
    fn paused_and_unavailable_positions_are_not_fabricated() {
        let paused = TransportState::Paused;
        assert!(progresses(None, None, paused, Duration::ZERO));
        assert!(!progresses(None, Some(0), paused, Duration::ZERO));
        assert!(progresses(Some(10), Some(10), paused, Duration::from_secs(30)));
        assert!(!progresses(Some(10), Some(11), paused, Duration::from_secs(30)));
    }
}
