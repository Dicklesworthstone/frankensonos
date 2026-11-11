//! Recover a confirmed renderer failure to the next already queued whole work.
//! Every mutation is sent once. Ownership and the selected occurrence are
//! checked between stages; a failed or ambiguous write relinquishes feeding.

use fsonos_core::playback::PlayerPlayback;
use fsonos_proto::content::{BrowseSnapshot, QUEUE, browse_range_snapshot};
use fsonos_proto::control as soap;
use fsonos_proto::play_mode::get_play_mode;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::{PlayMode, PlayerId, TransportState};
use serde::{Deserialize, Serialize};
use std::net::IpAddr;

use super::{FeedError, QueueFeed, QueuedWork, Speakers, host_of};

const MAX_ATTEMPTS: u8 = 3;
const WINDOW_SECS: i64 = 300;

/// A durable recovery budget. Old snapshots default to unused.
/// A backwards wall clock cannot replenish attempts in the current window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryBudget {
    attempts: Vec<i64>,
}

impl RecoveryBudget {
    fn reserve(&mut self, now: i64) -> Option<u8> {
        self.attempts.retain(|&at| {
            now.checked_sub(at)
                .is_none_or(|elapsed| elapsed < WINDOW_SECS)
        });
        if self.attempts.len() >= usize::from(MAX_ATTEMPTS) {
            return None;
        }
        self.attempts.push(now);
        u8::try_from(self.attempts.len()).ok()
    }
}

/// A recovery command that was sent, with its observed immediate outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryNotice {
    pub failed_work: String,
    pub next_work: String,
    pub attempt: u8,
    /// True only after read-back reports Playing and OK at the next work.
    /// A transitioning or newly failed renderer is not described as playing.
    pub playing: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Group {
    id: String,
    members: Vec<PlayerId>,
    boot: Option<u32>,
    generation: Option<u8>,
}

impl Group {
    pub(super) fn read<T: Transport + ?Sized>(
        at: &Speakers<'_, T>,
    ) -> Result<Option<Self>, FeedError> {
        let host = host_of(at)?;
        let topology = get_zone_group_state(at.transport, host)?;
        let mut groups = topology
            .groups
            .iter()
            .filter(|group| group.coordinator == *at.coordinator);
        let Some(group) = groups.next() else {
            return Ok(None);
        };
        if groups.next().is_some() {
            return Ok(None);
        }
        let Some(own) = group.members.iter().find(|member| {
            member.uuid == *at.coordinator && member.ip() == Some(host) && !member.is_zone_bridge
        }) else {
            return Ok(None);
        };
        let mut members: Vec<_> = group
            .members
            .iter()
            .filter(|member| !member.is_zone_bridge)
            .flat_map(|member| std::iter::once(member).chain(&member.satellites))
            .map(|member| member.uuid.clone())
            .collect();
        members.sort_by(|left, right| left.0.cmp(&right.0));
        members.dedup();
        // The daemon's cap wrapper uses this household snapshot. A changed
        // group must reach that snapshot before any automatic Play.
        let mut known = at
            .households
            .iter()
            .flat_map(|house| &house.groups)
            .filter(|group| group.coordinator == *at.coordinator);
        let Some(expected) = known.next() else {
            return Ok(None);
        };
        if known.next().is_some() {
            return Ok(None);
        }
        let mut expected = expected.members.clone();
        expected.sort_by(|left, right| left.0.cmp(&right.0));
        expected.dedup();
        Ok((members == expected).then_some(Self {
            id: group.id.clone(),
            members,
            boot: own.boot_seq,
            generation: own.sw_gen,
        }))
    }
}

struct Target {
    first: u32,
    tracks: Vec<String>,
    next: u32,
    next_uri: String,
    failed_title: String,
    next_title: String,
}

impl Target {
    fn choose(feed: &QueueFeed, position: u32, uri: &str) -> Result<Self, &'static str> {
        let works = || feed.played.iter().chain(&feed.queued);
        let current = works()
            .find(|work| {
                work.track_at(position)
                    .is_some_and(|track| same_track(track, uri))
            })
            .ok_or("the failed occurrence is no longer owned by this DJ")?;
        let first = intact(current).ok_or("the failed work was edited or is incomplete")?;
        let last = current.last().expect("an intact work has a final movement");
        let next = works()
            .filter(|work| work.first().is_some_and(|start| start > last))
            .min_by_key(|work| work.first())
            .ok_or("no complete DJ work is waiting after the failed work")?;
        let next_position = intact(next).ok_or("the next DJ work was edited or is incomplete")?;
        if last.checked_add(1) != Some(next_position) {
            return Err("the next DJ work is separated by items belonging to the listener");
        }
        Ok(Self {
            first,
            tracks: current
                .movements
                .iter()
                .chain(&next.movements)
                .map(|movement| movement.uri.clone())
                .collect(),
            next: next_position,
            next_uri: next.movements[0].uri.clone(),
            failed_title: current.title.clone(),
            next_title: next.title.clone(),
        })
    }

    fn read<T: Transport + ?Sized>(
        &self,
        at: &Speakers<'_, T>,
    ) -> Result<BrowseSnapshot, FeedError> {
        let count = u32::try_from(self.tracks.len())
            .map_err(|_| ProtoError::Malformed("DJ recovery range is too large".into()))?;
        Ok(browse_range_snapshot(
            at.transport,
            host_of(at)?,
            QUEUE,
            self.first - 1,
            count,
        )?)
    }

    fn matches(&self, snapshot: &BrowseSnapshot) -> bool {
        snapshot.objects.len() == self.tracks.len()
            && snapshot
                .objects
                .iter()
                .zip(&self.tracks)
                .all(|(object, uri)| {
                    object
                        .res
                        .as_ref()
                        .is_some_and(|resource| same_track(&resource.uri, uri))
                })
    }
}

fn intact(work: &QueuedWork) -> Option<u32> {
    let first = work
        .movements
        .first()?
        .position
        .filter(|&position| position > 0)?;
    work.movements
        .iter()
        .enumerate()
        .all(|(index, movement)| {
            movement.added
                && u32::try_from(index)
                    .ok()
                    .and_then(|index| first.checked_add(index))
                    .is_some_and(|position| movement.position == Some(position))
        })
        .then_some(first)
}

fn same_track(left: &str, right: &str) -> bool {
    match (track(left), track(right)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

/// Stored service URIs, bare enqueue URIs and renderer URIs name the same
/// track. A substring in an unrelated source is never an owned identity.
fn track(value: &str) -> Option<String> {
    let value = value
        .strip_prefix("x-sonos-spotify:")
        .unwrap_or(value)
        .split('?')
        .next()?;
    let decoded = value.replace("%3a", ":").replace("%3A", ":");
    let id = decoded.strip_prefix("spotify:track:")?;
    (!id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._~".contains(&byte)))
    .then_some(decoded)
}

fn occurrence<T: Transport + ?Sized>(
    at: &Speakers<'_, T>,
    host: IpAddr,
    position: u32,
    uri: &str,
    length: u32,
) -> Result<Option<soap::TransportInfo>, FeedError> {
    let own_queue = format!("x-rincon-queue:{}#0", at.coordinator.0);
    let media = soap::get_media_info(at.transport, host)?;
    if media.uri != own_queue || media.tracks != length {
        return Ok(None);
    }
    let current = soap::get_position_info(at.transport, host)?;
    if current.track != position || !same_track(&current.uri, uri) {
        return Ok(None);
    }
    let transport = soap::get_transport_info(at.transport, host)?;
    let after = soap::get_position_info(at.transport, host)?;
    if after.track != current.track
        || !same_track(&after.uri, &current.uri)
        || (transport.state != TransportState::Playing
            && after.position_secs != current.position_secs)
        || soap::get_transport_info(at.transport, host)? != transport
    {
        return Ok(None);
    }
    let media = soap::get_media_info(at.transport, host)?;
    Ok((media.uri == own_queue && media.tracks == length).then_some(transport))
}

fn stopped_error(transport: &soap::TransportInfo) -> bool {
    transport.state == TransportState::Stopped && transport.status == "ERROR_OCCURRED"
}

fn halt(feed: &mut QueueFeed, detail: impl Into<String>) -> FeedError {
    feed.release();
    FeedError::RecoveryStopped(detail.into())
}

// These observations are checked before writes; the device does not provide
// a transaction that excludes later changes from another controller.
struct Proof<'a> {
    group: &'a Group,
    queue: &'a BrowseSnapshot,
    target: &'a Target,
    length: u32,
}

impl Proof<'_> {
    fn unchanged<T: Transport + ?Sized>(&self, at: &Speakers<'_, T>) -> Result<bool, FeedError> {
        Ok(Group::read(at)?.as_ref() == Some(self.group) && self.target.read(at)? == *self.queue)
    }
}

/// Advance a single confirmed attempt without replaying any mutation.
/// An ambiguous reply ends management; observation alone cannot undo it.
fn start_next<T: Transport + ?Sized>(
    at: &Speakers<'_, T>,
    proof: &Proof<'_>,
    normal: bool,
    position: u32,
    uri: &str,
) -> Result<soap::TransportInfo, String> {
    let host = host_of(at).map_err(|error| error.to_string())?;
    let length = proof.length;
    if !normal {
        soap::set_normal_play_mode(at.transport, host).map_err(|error| {
            format!("SetPlayMode was not confirmed ({error}); it will not be replayed")
        })?;
        let ready = (|| {
            Ok::<_, FeedError>(
                proof.unchanged(at)?
                    && get_play_mode(at.transport, host)? == PlayMode::Normal
                    && occurrence(at, host, position, uri, length)?
                        .as_ref()
                        .is_some_and(stopped_error),
            )
        })();
        if !matches!(ready, Ok(true)) {
            return Err("the failed occurrence changed after setting queue order; \
                 recovery was not continued"
                .into());
        }
    }
    let target = proof.target;
    soap::seek_track(at.transport, host, target.next)
        .map_err(|error| format!("Seek was not confirmed ({error}); it will not be replayed"))?;
    let selected = (|| {
        Ok::<_, FeedError>(
            proof.unchanged(at)?
                && get_play_mode(at.transport, host)? == PlayMode::Normal
                && occurrence(at, host, target.next, &target.next_uri, proof.length)?
                    .is_some_and(|transport| transport.state == TransportState::Stopped),
        )
    })();
    if !matches!(selected, Ok(true)) {
        return Err(
            "the next whole work could not be confirmed selected; Play was not sent".into(),
        );
    }
    soap::play(at.transport, host)
        .map_err(|error| format!("Play was not confirmed ({error}); it will not be replayed"))?;
    let after = (|| {
        if !proof.unchanged(at)? {
            return Ok(None);
        }
        occurrence(at, host, target.next, &target.next_uri, proof.length)
    })();
    let Ok(Some(after)) = after else {
        return Err("the recovery outcome could not be confirmed; \
             the queue and playback were left alone"
            .into());
    };
    if !matches!(
        after.state,
        TransportState::Playing | TransportState::Transitioning
    ) && !stopped_error(&after)
    {
        return Err(
            "the player paused or stopped during recovery; automatic playback was not repeated"
                .into(),
        );
    }
    Ok(after)
}

/// A stale or still-playing error is not permission to move a queue.
/// It never falls through to ordinary feeding, history or exhaustion.
pub(super) fn recover<T: Transport + ?Sized>(
    feed: &mut QueueFeed,
    at: &Speakers<'_, T>,
    event: &PlayerPlayback,
    now: i64,
) -> Result<(), FeedError> {
    if feed.coordinator() != *at.coordinator {
        return Ok(());
    }
    let Some(position) = event.queue_position.filter(|&position| position > 0) else {
        return Ok(());
    };
    let Some(uri) = event.track_uri.as_deref() else {
        return Ok(());
    };
    let Some(length) = feed.left_at() else {
        return Ok(());
    };
    if event
        .queue_length
        .is_some_and(|observed| observed != length)
    {
        return Ok(());
    }
    let host = host_of(at)?;
    let Some(group) = Group::read(at)? else {
        return Ok(());
    };
    if !occurrence(at, host, position, uri, length)?
        .as_ref()
        .is_some_and(stopped_error)
    {
        return Ok(());
    }
    let target = Target::choose(feed, position, uri).map_err(|detail| halt(feed, detail))?;
    let queue = target.read(at)?;
    if !target.matches(&queue) {
        return Err(halt(
            feed,
            "the owned queue occurrences changed before recovery",
        ));
    }
    let proof = Proof {
        group: &group,
        queue: &queue,
        target: &target,
        length,
    };
    let normal = get_play_mode(at.transport, host)? == PlayMode::Normal;
    if !proof.unchanged(at)?
        || !occurrence(at, host, position, uri, length)?
            .as_ref()
            .is_some_and(stopped_error)
    {
        return Ok(());
    }
    // Reserve before any mutation. Exhaustion of the budget does not stop,
    // clear, seek or repeatedly retry the player's queue.
    let Some(attempt) = feed.recovery.reserve(now) else {
        return Err(halt(
            feed,
            "three automatic recoveries were attempted within five minutes; \
             inspect the speaker and start the DJ again",
        ));
    };
    feed.dry_end = None;
    let after =
        start_next(at, &proof, normal, position, uri).map_err(|detail| halt(feed, detail))?;
    feed.playing = Some(target.next);
    feed.jumped = true;
    feed.recovery_notice = Some(RecoveryNotice {
        failed_work: target.failed_title,
        next_work: target.next_title,
        attempt,
        playing: after.state == TransportState::Playing && after.status == "OK",
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_enqueued_and_rendered_track_identities_match_exactly() {
        let plain = "spotify:track:ExampleTrack123";
        for value in [
            plain,
            "spotify%3atrack%3aExampleTrack123",
            "x-sonos-spotify:spotify%3atrack%3aExampleTrack123?sid=9",
            "x-sonos-spotify:spotify:track:ExampleTrack123?sid=9",
        ] {
            assert!(same_track(plain, value), "{value}");
            assert!(same_track(value, plain), "{value}");
        }
        assert!(same_track(
            "spotify:track:0-1-2",
            "x-sonos-spotify:spotify%3atrack%3a0-1-2?sid=9"
        ));
        for value in [
            "spotify:track:OtherTrack",
            "spotify:album:ExampleTrack123",
            "spotify:track:",
            "https://example.invalid/spotify:track:ExampleTrack123",
            "x-sonos-vli:player:2,spotify:ExampleTrack123",
        ] {
            assert!(!same_track(plain, value), "{value}");
        }
    }

    #[test]
    fn rolling_budget_is_durable_and_clock_regression_cannot_reset_it() {
        let mut budget = RecoveryBudget::default();
        assert_eq!(budget.reserve(1000), Some(1));
        assert_eq!(budget.reserve(1001), Some(2));
        let json = serde_json::to_string(&budget).unwrap();
        let mut budget: RecoveryBudget = serde_json::from_str(&json).unwrap();
        assert_eq!(budget.reserve(1002), Some(3));
        assert_eq!(budget.reserve(1299), None);
        assert_eq!(budget.reserve(900), None);
        assert_eq!(budget.reserve(1300), Some(3));
        assert_eq!(budget.reserve(1300), None);
        assert_eq!(budget.reserve(1600), Some(1));
    }

    #[test]
    fn older_saved_feeds_default_to_an_unused_recovery_budget() {
        let legacy = serde_json::json!({
            "zone": "test-coordinator",
            "earlier": [],
            "lookahead": 1,
            "queued": [],
            "played": [],
            "playing": null,
            "recorded": null
        });
        let snapshot: super::super::FeedSnapshot = serde_json::from_value(legacy).unwrap();
        assert_eq!(snapshot.recovery, RecoveryBudget::default());
    }

    #[test]
    fn a_window_boundary_does_not_allow_a_burst_of_recovery_attempts() {
        let mut budget = RecoveryBudget::default();
        for (at, expected) in [
            (0, Some(1)),
            (299, Some(2)),
            (299, Some(3)),
            (300, Some(3)),
            (300, None),
            (300, None),
            (599, Some(2)),
        ] {
            assert_eq!(budget.reserve(at), expected);
        }
    }
}
