//! The ordinary survey's read-only playback sweep, off the event thread.
//! One queued job per renderer; shared admission with fast clock recovery.
//! Services fail and become stale independently, so a volume event cannot
//! suppress a valid transport sample, nor can a failed read erase live state.

use super::{Engine, Model, observation};
use crate::events::{Service, wanted};
use crate::live::LiveEvent;
use crate::playback::{Changes, EventSource, Playback, PlaybackObservation, Stamp};
use crate::reconcile::Health;
use fsonos_proto::control::get_group_volume;
use fsonos_proto::play_mode::get_play_mode;
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::{Generation, HouseholdId, PlayMode, PlayerId, ZoneGroup};
use std::collections::{HashSet, VecDeque};
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Instant;

type Part<T> = Result<Option<T>, ProtoError>;

#[derive(Clone, PartialEq, Eq)]
struct Target {
    player: PlayerId,
    host: IpAddr,
    household: Option<HouseholdId>,
    generation: Generation,
    /// Only a coordinator has group-wide transport and rendering reads.
    group: Option<ZoneGroup>,
}

impl Target {
    fn of(model: &Model, player: &PlayerId) -> Option<Self> {
        if model.rec.health.of(player).is_some_and(|h| h.health == Health::Offline) {
            return None;
        }
        model.rec.households.iter().find_map(|house| {
            let found = house.player(player)?;
            Some(Self {
                player: player.clone(),
                host: found.ip,
                household: house.id.clone(),
                generation: found.generation,
                group: house.groups.iter().find(|g| g.coordinator == *player).cloned(),
            })
        })
    }
}

#[derive(Clone)]
struct Stamps {
    transport: Option<Stamp>,
    rendering: Option<Stamp>,
    group: Option<Stamp>,
}

impl Stamps {
    fn of(playback: &Playback, player: &PlayerId) -> Self {
        Self {
            transport: playback.observation_stamp(player, EventSource::AvTransport),
            rendering: playback.observation_stamp(player, EventSource::RenderingControl),
            group: playback.observation_stamp(player, EventSource::GroupRenderingControl),
        }
    }
}

struct Sample {
    levels: Part<(u8, bool)>,
    transport: Part<(PlaybackObservation, Instant)>,
    group_volume: Part<u8>,
    mode: Part<PlayMode>,
}

fn one<T>(stopping: &impl Fn() -> bool, read: impl FnOnce() -> Result<T, ProtoError>) -> Part<T> {
    if stopping() {
        return Ok(None);
    }
    let value = read()?;
    Ok((!stopping()).then_some(value))
}

fn read(transport: &dyn Transport, target: &Target, stopping: impl Fn() -> bool) -> Sample {
    let host = target.host;
    let levels = observation::room_levels(transport, host, &stopping);
    if target.group.is_none() {
        return Sample {
            levels,
            transport: Ok(None),
            group_volume: Ok(None),
            mode: Ok(None),
        };
    }
    // Do not use `?` between services: one unavailable SOAP service must not
    // suppress the other services' valid results. Check cancellation per call.
    Sample {
        levels,
        transport: observation::read(transport, host, &stopping),
        group_volume: one(&stopping, || get_group_volume(transport, host)),
        mode: one(&stopping, || get_play_mode(transport, host)),
    }
}

struct Pending {
    target: Target,
    stamps: Stamps,
    cancelled: Arc<AtomicBool>,
    thread: JoinHandle<Sample>,
}

#[derive(Default)]
pub(super) struct Sweep {
    queue: VecDeque<Target>,
    pending: Vec<Pending>,
}

impl Sweep {
    pub(super) fn contains(&self, player: &PlayerId) -> bool {
        self.pending.iter().any(|read| read.target.player == *player)
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

impl Drop for Sweep {
    fn drop(&mut self) {
        self.cancel_all();
    }
}

pub(super) fn invalidate(model: &Model) {
    for read in &model.progress.sweep.pending {
        if Target::of(model, &read.target.player).as_ref() != Some(&read.target) {
            read.cancelled.store(true, Ordering::Release);
        }
    }
}

/// Keep waiting rooms in order, coalesce repeated surveys, and drop vanished
/// targets. The queue is bounded by current inventory, not by survey count.
pub(super) fn request(model: &mut Model) {
    invalidate(model);
    let mut seen = HashSet::new();
    let targets: Vec<_> = wanted(&model.rec.households)
        .into_iter()
        .filter(|want| want.service == Service::RenderingControl)
        .filter(|want| seen.insert(want.player.clone()))
        .filter_map(|want| Target::of(model, &want.player))
        .collect();
    let sweep = &mut model.progress.sweep;
    sweep.queue.retain(|target| targets.contains(target));
    for target in targets {
        if !sweep.queue.contains(&target)
            && !sweep.pending.iter().any(|read| read.target == target)
        {
            sweep.queue.push_back(target);
        }
    }
}

/// Start at most one renderer, without waiting on its network calls. A stuck
/// or cancelled worker retains its shared slot until it actually exits.
pub(super) fn start_next(engine: &Engine, model: &mut Model) -> bool {
    if engine.stop.load(Ordering::Acquire) || !model.progress.has_capacity() {
        return false;
    }
    let index = model.progress.sweep.queue.iter().position(|target| {
        !model.progress.busy(&target.player)
    });
    let Some(target) = index.and_then(|index| model.progress.sweep.queue.remove(index)) else {
        return false;
    };
    if Target::of(model, &target.player).as_ref() != Some(&target) {
        return false;
    }
    let stamps = Stamps::of(&model.playback, &target.player);
    let transport = Arc::clone(&engine.transport);
    let stopping = Arc::clone(&engine.stop);
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancellation = Arc::clone(&cancelled);
    let at = target.clone();
    let spawned = std::thread::Builder::new()
        .name("fsonos-survey-read".into())
        .spawn(move || {
            read(&*transport, &at, || {
                stopping.load(Ordering::Acquire) || cancellation.load(Ordering::Acquire)
            })
        });
    match spawned {
        Ok(thread) => model.progress.sweep.pending.push(Pending {
            target,
            stamps,
            cancelled,
            thread,
        }),
        Err(error) => {
            // Match an ordinary failed survey read: the next survey retries.
            // Do not create a hot spawn loop or an unbounded retry queue.
            tracing::warn!(%error, "cannot start survey read; retaining live state");
        }
    }
    true
}

fn accepted<T>(part: Part<T>, current: bool, service: &str) -> Option<T> {
    match part {
        Ok(value) if current => value,
        Err(error) => {
            tracing::debug!(%error, service, "survey read failed; retaining live state");
            None
        }
        _ => None,
    }
}

fn record(events: &mut Vec<LiveEvent>, player: &PlayerId, changes: Changes) {
    if !changes.is_empty() {
        events.push(LiveEvent::Playback {
            player: player.clone(),
            changes,
        });
    }
}

pub(super) fn complete(engine: &Engine, model: &mut Model) {
    invalidate(model);
    if engine.stop.load(Ordering::Acquire) {
        model.progress.sweep.cancel_all();
    }
    let mut events = Vec::new();
    for pending in model.progress.sweep.ready() {
        let cancelled = pending.cancelled.load(Ordering::Acquire);
        let Ok(sample) = pending.thread.join() else {
            tracing::warn!("survey read worker panicked; retaining live state");
            continue;
        };
        if cancelled || engine.stop.load(Ordering::Acquire) {
            continue;
        }
        let player = &pending.target.player;
        let now = Stamps::of(&model.playback, player);
        // Decide before applying any part: transport and play mode share an
        // AVTransport stamp, and each accepted read advances that stamp.
        let av_current = now.transport == pending.stamps.transport;
        if let Some((volume, mute)) = accepted(
            sample.levels,
            now.rendering == pending.stamps.rendering,
            "RenderingControl",
        ) {
            let changes = model.playback.apply_room_levels(player, volume, mute);
            record(&mut events, player, changes);
        }
        if let Some((sample, observed)) = accepted(sample.transport, av_current, "AVTransport") {
            let changes = model.playback.apply_observation(player, &sample, observed);
            // A corrected position is observable even without a track change.
            events.push(LiveEvent::Playback {
                player: player.clone(),
                changes,
            });
        }
        if let Some(volume) = accepted(
            sample.group_volume,
            now.group == pending.stamps.group,
            "GroupRenderingControl",
        ) {
            let changes = model.playback.apply_group_volume(player, volume);
            record(&mut events, player, changes);
        }
        if let Some(mode) = accepted(sample.mode, av_current, "transport settings") {
            let changes = model.playback.apply_play_mode(player, mode);
            record(&mut events, player, changes);
        }
    }
    if !events.is_empty() {
        engine.publish(model, events);
    }
}

#[cfg(test)]
mod tests;
