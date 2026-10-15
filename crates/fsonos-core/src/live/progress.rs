//! Repair a missed track-end event without polling normally advancing music.

use super::{Engine, LiveEvent, Model, observation};
use crate::playback::{Playback, PlayerPlayback};
use fsonos_types::PlayerId;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// Allow ordinary NOTIFY delivery and small position-estimate errors first.
const END_GRACE: Duration = Duration::from_secs(5);
const RETRY_MIN: Duration = Duration::from_secs(30);
const RETRY_MAX: Duration = Duration::from_secs(300);

#[derive(Clone, Debug, PartialEq, Eq)]
struct Identity {
    uri: String,
    slot: Option<u32>,
}

impl Identity {
    fn of(state: &PlayerPlayback) -> Option<Self> {
        Some(Self {
            uri: state.track_uri.clone()?,
            slot: state.queue_position.filter(|&n| n > 0),
        })
    }
}

struct Attempt {
    identity: Identity,
    retry_at: Instant,
    failures: u32,
}

struct Probe {
    player: PlayerId,
    host: IpAddr,
    identity: Identity,
}

/// Bounded per-coordinator attempts; entries disappear with their identity.
#[derive(Default)]
pub(super) struct Watchdog {
    attempts: HashMap<PlayerId, Attempt>,
}

impl Watchdog {
    /// Oldest overdue finite track, at most one per live-worker turn. The
    /// usual event stream continually moves the predicted end forward, so
    /// healthy playback makes no additional network requests.
    fn next(
        &mut self,
        coordinators: &[(PlayerId, IpAddr)],
        playback: &Playback,
        now: Instant,
    ) -> Option<Probe> {
        self.attempts.retain(|id, attempt| {
            coordinators.iter().any(|(player, _)| player == id)
                && playback.of(id).is_some_and(|state| {
                    state.expected_end().is_some()
                        && Identity::of(state).as_ref() == Some(&attempt.identity)
                })
        });
        coordinators
            .iter()
            .filter_map(|(player, host)| {
                let state = playback.of(player)?;
                let identity = Identity::of(state)?;
                let mut due = state.expected_end()?.checked_add(END_GRACE)?;
                if let Some(attempt) = self.attempts.get(player) {
                    due = due.max(attempt.retry_at);
                }
                (now >= due).then_some((
                    due,
                    Probe {
                        player: player.clone(),
                        host: *host,
                        identity,
                    },
                ))
            })
            .min_by(|(a, left), (b, right)| {
                a.cmp(b).then_with(|| left.player.0.cmp(&right.player.0))
            })
            .map(|(_, probe)| probe)
    }

    fn completed(&mut self, probe: Probe, success: bool, now: Instant) {
        let failures = if success {
            0
        } else {
            self.attempts
                .get(&probe.player)
                .filter(|previous| previous.identity == probe.identity)
                .map_or(1, |previous| previous.failures.saturating_add(1))
        };
        // Even successful reads of a stuck or repeating last track have a
        // cooldown. Network time is not subtracted from the retry interval.
        let factor = 1_u32 << failures.saturating_sub(1).min(4);
        let delay = (RETRY_MIN * factor).min(RETRY_MAX);
        self.attempts.insert(
            probe.player,
            Attempt {
                identity: probe.identity,
                retry_at: now + delay,
                failures,
            },
        );
    }
}

/// This path only reads. It never fabricates a STOPPED event or issues Play,
/// Next, Seek or queue edits. The normal DJ follower sees recovered changes
/// and retains its own ownership, policy and indeterminate-write safeguards.
pub(super) fn recover(engine: &Engine, model: &mut Model) {
    if engine.stop.load(Ordering::Acquire) {
        return;
    }
    let coordinators = observation::coordinators(model);
    let Some(probe) = model
        .progress
        .next(&coordinators, &model.playback, Instant::now())
    else {
        return;
    };
    match observation::read(&engine.via(), probe.host, || {
        engine.stop.load(Ordering::Acquire)
    }) {
        Ok(Some((observation, observed))) => {
            let changes = model
                .playback
                .apply_observation(&probe.player, &observation, observed);
            let events = if changes.is_empty() {
                Vec::new()
            } else {
                vec![LiveEvent::Playback {
                    player: probe.player.clone(),
                    changes,
                }]
            };
            model.progress.completed(probe, true, Instant::now());
            engine.publish(model, events);
        }
        Ok(None) => {}
        Err(error) => {
            tracing::debug!(%error, "overdue track read-back failed; retrying with backoff");
            model.progress.completed(probe, false, Instant::now());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playback::PlaybackObservation;
    use fsonos_proto::control::{MediaInfo, PositionInfo, TransportInfo};
    use fsonos_types::TransportState;

    fn observation(state: TransportState, slot: u32) -> PlaybackObservation {
        PlaybackObservation {
            transport: TransportInfo {
                state,
                status: "OK".into(),
            },
            position: PositionInfo {
                track: slot,
                duration_secs: Some(180),
                position_secs: Some(170),
                uri: "test:repeated-recording".into(),
                metadata: None,
            },
            media: MediaInfo {
                tracks: 8,
                uri: "test:queue".into(),
                uri_metadata: String::new(),
            },
        }
    }

    fn sample(playback: &mut Playback, player: &PlayerId, slot: u32, now: Instant) {
        let playing = observation(TransportState::Playing, slot);
        playback.apply_observation(player, &playing, now);
    }

    fn targets() -> Vec<(PlayerId, IpAddr)> {
        vec![
            (PlayerId("test-a".into()), "192.0.2.1".parse().unwrap()),
            (PlayerId("test-b".into()), "192.0.2.2".parse().unwrap()),
        ]
    }

    #[test]
    fn normal_progress_and_grace_do_not_poll() {
        let now = Instant::now();
        let targets = targets();
        let mut playback = Playback::default();
        let mut watchdog = Watchdog::default();
        sample(&mut playback, &targets[0].0, 1, now);
        assert!(
            watchdog
                .next(&targets, &playback, now + Duration::from_secs(14))
                .is_none()
        );
        sample(
            &mut playback,
            &targets[0].0,
            2,
            now + Duration::from_secs(10),
        );
        assert!(
            watchdog
                .next(&targets, &playback, now + Duration::from_secs(24))
                .is_none()
        );
        assert!(watchdog.attempts.is_empty());
        assert!(
            watchdog
                .next(&targets, &playback, now + Duration::from_secs(25))
                .is_some()
        );
    }

    #[test]
    fn failures_back_off_independently_and_success_resets_them() {
        let start = Instant::now();
        let targets = targets();
        let mut playback = Playback::default();
        let mut watchdog = Watchdog::default();
        sample(&mut playback, &targets[0].0, 1, start);
        sample(&mut playback, &targets[1].0, 1, start);
        let mut now = start + Duration::from_secs(15);
        let first = watchdog.next(&targets, &playback, now).unwrap();
        assert_eq!(first.player, targets[0].0);
        watchdog.completed(first, false, now);
        let other = watchdog.next(&targets, &playback, now).unwrap();
        assert_eq!(
            other.player, targets[1].0,
            "one failing room cannot starve another"
        );
        watchdog.completed(other, true, now);
        for delay in [30, 60, 120, 240, 300, 300] {
            let due = watchdog.attempts[&targets[0].0].retry_at;
            assert_eq!(due.duration_since(now), Duration::from_secs(delay));
            assert!(
                watchdog
                    .next(
                        &targets[..1],
                        &playback,
                        due.checked_sub(Duration::from_nanos(1)).unwrap()
                    )
                    .is_none()
            );
            let probe = watchdog.next(&targets[..1], &playback, due).unwrap();
            watchdog.completed(probe, false, due);
            now = due;
        }
        let due = watchdog.attempts[&targets[0].0].retry_at;
        let probe = watchdog.next(&targets[..1], &playback, due).unwrap();
        watchdog.completed(probe, true, due);
        assert_eq!(watchdog.attempts[&targets[0].0].failures, 0);
        assert_eq!(watchdog.attempts[&targets[0].0].retry_at, due + RETRY_MIN);
    }

    #[test]
    fn a_new_queue_slot_does_not_inherit_the_same_uris_backoff() {
        let start = Instant::now();
        let targets = targets();
        let mut playback = Playback::default();
        let mut watchdog = Watchdog::default();
        sample(&mut playback, &targets[0].0, 1, start);
        let now = start + Duration::from_secs(15);
        let probe = watchdog.next(&targets, &playback, now).unwrap();
        watchdog.completed(probe, false, now);
        sample(&mut playback, &targets[0].0, 2, start);
        let probe = watchdog.next(&targets, &playback, now).unwrap();
        assert_eq!(probe.identity.slot, Some(2));
        assert!(watchdog.attempts.is_empty());
    }

    #[test]
    fn paused_streaming_unknown_and_removed_players_have_no_fast_probes() {
        let start = Instant::now();
        let targets = targets();
        let mut playback = Playback::default();
        let mut watchdog = Watchdog::default();
        let now = start + Duration::from_secs(1000);
        assert!(watchdog.next(&targets, &playback, now).is_none());
        sample(&mut playback, &targets[0].0, 1, start);
        let probe = watchdog.next(&targets, &playback, now).unwrap();
        watchdog.completed(probe, false, now);
        assert!(watchdog.next(&[], &playback, now).is_none());
        assert!(watchdog.attempts.is_empty());
        playback.remove(&targets[0].0);
        assert!(watchdog.next(&targets, &playback, now).is_none());
        for state in [
            TransportState::Paused,
            TransportState::Stopped,
            TransportState::Unknown,
        ] {
            sample(&mut playback, &targets[0].0, 1, start);
            let paused = observation(state, 1);
            playback.apply_observation(&targets[0].0, &paused, start);
            assert!(watchdog.next(&targets, &playback, now).is_none());
        }
        let mut stream = observation(TransportState::Playing, 0);
        stream.position.uri = "test:radio".into();
        stream.position.duration_secs = Some(0);
        playback.apply_observation(&targets[0].0, &stream, start);
        assert!(watchdog.next(&targets, &playback, now).is_none());
    }
}
