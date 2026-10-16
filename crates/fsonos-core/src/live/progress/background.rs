//! Read clocks without holding up GENA intake. Only the live-model thread
//! publishes results; workers own no model, subscriptions or playback writes.

use super::{Engine, Model, Probe, observation};
use crate::live::LiveEvent;
use crate::playback::{EventSource, PlaybackObservation, PlayerPlayback, Stamp};
use fsonos_proto::ProtoError;
use fsonos_types::{HouseholdId, PlayerId, ZoneGroup};
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Instant;

/// One slow room must not block every other room. Never replace a stuck
/// worker with another: its slot remains occupied until it actually exits.
pub(super) const MAX_READS: usize = 4;

type ReadResult = Result<Option<(PlaybackObservation, Instant)>, ProtoError>;

#[derive(Clone, PartialEq, Eq)]
struct Authority {
    household: Option<HouseholdId>,
    group: ZoneGroup,
    host: IpAddr,
}

impl Authority {
    fn of(model: &Model, player: &PlayerId) -> Option<Self> {
        let host = observation::coordinators(model)
            .into_iter()
            .find_map(|(id, host)| (id == *player).then_some(host))?;
        model.rec.households.iter().find_map(|house| {
            let group = house
                .groups
                .iter()
                .find(|group| group.coordinator == *player)?;
            (house.player(player)?.ip == host).then(|| Self {
                household: house.id.clone(),
                group: group.clone(),
                host,
            })
        })
    }
}

struct Pending {
    probe: Probe,
    baseline: PlayerPlayback,
    stamp: Option<Stamp>,
    authority: Authority,
    cancelled: Arc<AtomicBool>,
    thread: JoinHandle<ReadResult>,
}

impl Pending {
    fn current(&self, model: &Model) -> bool {
        !self.cancelled.load(Ordering::Acquire)
            && model
                .playback
                .of(&self.probe.player)
                .is_some_and(|state| state.same_transport_observation(&self.baseline))
            && model
                .playback
                .observation_stamp(&self.probe.player, EventSource::AvTransport)
                == self.stamp
            && Authority::of(model, &self.probe.player).as_ref() == Some(&self.authority)
    }
}

#[derive(Default)]
pub(super) struct Reads {
    pending: Vec<Pending>,
}

impl Reads {
    pub(super) fn contains(&self, player: &PlayerId) -> bool {
        self.pending.iter().any(|read| read.probe.player == *player)
    }

    pub(super) fn len(&self) -> usize {
        self.pending.len()
    }

    pub(super) fn cancel_all(&self) {
        for read in &self.pending {
            read.cancelled.store(true, Ordering::Release);
        }
    }

    fn ready(&mut self) -> Vec<Pending> {
        let mut ready = Vec::new();
        let mut index = 0;
        while index < self.pending.len() {
            if self.pending[index].thread.is_finished() {
                ready.push(self.pending.swap_remove(index));
            } else {
                index += 1;
            }
        }
        ready
    }
}

impl Drop for Reads {
    fn drop(&mut self) {
        // A Transport call already in progress has its own deadline. Do not
        // join it here, and do not issue the remaining SOAP calls after it.
        self.cancel_all();
    }
}

pub(super) fn start(engine: &Engine, model: &mut Model, probe: Probe) {
    if engine.stop.load(Ordering::Acquire)
        || !model.progress.has_capacity()
        || model.progress.busy(&probe.player)
    {
        return;
    }
    let Some(authority) = Authority::of(model, &probe.player) else {
        return;
    };
    if authority.host != probe.host {
        return;
    }
    let Some(baseline) = model.playback.begin_position_sample(&probe.player) else {
        return;
    };
    let stamp = model
        .playback
        .observation_stamp(&probe.player, EventSource::AvTransport);
    let transport = Arc::clone(&engine.transport);
    let stopping = Arc::clone(&engine.stop);
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancellation = Arc::clone(&cancelled);
    let host = probe.host;
    let spawned = std::thread::Builder::new()
        .name("fsonos-playback-read".into())
        .spawn(move || {
            observation::read(&*transport, host, || {
                stopping.load(Ordering::Acquire) || cancellation.load(Ordering::Acquire)
            })
        });
    match spawned {
        Ok(thread) => model.progress.reads.pending.push(Pending {
            probe,
            baseline,
            stamp,
            authority,
            cancelled,
            thread,
        }),
        Err(error) => {
            tracing::debug!(%error, "cannot start playback read; retrying with backoff");
            retry_request(model, &probe);
            model.progress.completed(probe, false, Instant::now());
        }
    }
}

fn retry_request(model: &mut Model, probe: &Probe) {
    if probe.position_requested {
        model.playback.request_position_sample(&probe.player);
    }
}

/// Invalidate on each accepted event, including intermediate regrouping.
pub(super) fn invalidate(model: &Model) {
    // Cancelling is monotonic: a group which disappears and later returns
    // cannot make an already-invalidated result authoritative again.
    for read in &model.progress.reads.pending {
        if !read.current(model) {
            read.cancelled.store(true, Ordering::Release);
        }
    }
}

pub(super) fn complete(engine: &Engine, model: &mut Model) {
    if engine.stop.load(Ordering::Acquire) {
        model.progress.reads.cancel_all();
    }
    invalidate(model);
    let mut events = Vec::new();
    let mut publish = false;
    for read in model.progress.reads.ready() {
        let current = read.current(model);
        // ready() removed only finished handles. Joining cannot wait for I/O.
        let result = read
            .thread
            .join()
            .unwrap_or(Err(ProtoError::NotWired("playback read worker panicked")));
        let probe = read.probe;
        let now = Instant::now();
        if !current {
            retry_request(model, &probe);
            // Keep normal coalescing, without charging a new GENA observation
            // as a network failure. No returned playback state is applied.
            model.progress.completed(probe, true, now);
            continue;
        }
        match result {
            Ok(Some((observation, observed))) => {
                let changes =
                    model
                        .playback
                        .apply_observation(&probe.player, &observation, observed);
                // Even an unchanged identity needs a position invalidation
                // after a seek, but never an invented DJ track transition.
                if !changes.is_empty() || probe.position_requested {
                    events.push(LiveEvent::Playback {
                        player: probe.player.clone(),
                        changes,
                    });
                }
                publish = true;
                model.progress.completed(probe, true, now);
            }
            Ok(None) => {
                retry_request(model, &probe);
                model.progress.completed(probe, true, now);
            }
            Err(error) => {
                tracing::debug!(%error, "playback read-back failed; retrying with backoff");
                retry_request(model, &probe);
                model.progress.completed(probe, false, now);
            }
        }
    }
    if publish {
        engine.publish(model, events);
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod observation_tests {
    use super::*;
    use fsonos_proto::control::PositionInfo;

    #[test]
    fn a_newer_clockless_read_invalidates_an_equal_transport_baseline() {
        let player = PlayerId("test-clockless".into());
        let mut playback = crate::playback::Playback::default();
        let sample = PositionInfo {
            track: 0,
            duration_secs: None,
            position_secs: None,
            uri: "test:radio".into(),
            metadata: None,
        };
        playback.apply_position(&player, &sample, Instant::now());
        let baseline = playback.begin_position_sample(&player).unwrap();
        let stamp = playback.observation_stamp(&player, EventSource::AvTransport);
        playback.apply_position(&player, &sample, Instant::now());
        assert!(playback.of(&player).unwrap().same_transport_observation(&baseline));
        assert_ne!(playback.observation_stamp(&player, EventSource::AvTransport), stamp);
    }
}
