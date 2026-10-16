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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
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
        if model
            .rec
            .health
            .of(player)
            .is_some_and(|h| h.health == Health::Offline)
        {
            return None;
        }
        model.rec.households.iter().find_map(|house| {
            let found = house.player(player)?;
            Some(Self {
                player: player.clone(),
                host: found.ip,
                household: house.id.clone(),
                generation: found.generation,
                group: house
                    .groups
                    .iter()
                    .find(|g| g.coordinator == *player)
                    .cloned(),
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

impl Default for Sample {
    fn default() -> Self {
        Self {
            levels: Ok(None),
            transport: Ok(None),
            group_volume: Ok(None),
            mode: Ok(None),
        }
    }
}

/// A fixed, single-use slot for each service, not an unbounded message queue.
/// Never hold the mailbox lock while reading the network. The model can take
/// completed services even while this player's next service is still blocked.
fn read(
    transport: &dyn Transport,
    target: &Target,
    stopping: impl Fn() -> bool,
    sample: &Mutex<Sample>,
) {
    let host = target.host;
    let levels = observation::room_levels(transport, host, &stopping);
    sample.lock().unwrap_or_else(PoisonError::into_inner).levels = levels;
    if target.group.is_none() || stopping() {
        return;
    }
    // One unavailable service does not suppress the other services' results.
    let playback = observation::read(transport, host, &stopping);
    sample.lock().unwrap_or_else(PoisonError::into_inner).transport = playback;
    let volume = one(&stopping, || get_group_volume(transport, host));
    sample
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .group_volume = volume;
    let mode = one(&stopping, || get_play_mode(transport, host));
    sample.lock().unwrap_or_else(PoisonError::into_inner).mode = mode;
}

struct Pending {
    target: Target,
    stamps: Stamps,
    cancelled: Arc<AtomicBool>,
    sample: Arc<Mutex<Sample>>,
    thread: JoinHandle<()>,
}

#[derive(Default)]
pub(super) struct Sweep {
    queue: VecDeque<Target>,
    pending: Vec<Pending>,
}

impl Sweep {
    pub(super) fn contains(&self, player: &PlayerId) -> bool {
        self.pending
            .iter()
            .any(|read| read.target.player == *player)
    }

    pub(super) fn len(&self) -> usize {
        self.pending.len()
    }

    pub(super) fn cancel_all(&self) {
        for read in &self.pending {
            read.cancelled.store(true, Ordering::Release);
        }
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
        if !sweep.queue.contains(&target) && !sweep.pending.iter().any(|read| read.target == target)
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
    let index = model
        .progress
        .sweep
        .queue
        .iter()
        .position(|target| !model.progress.busy(&target.player));
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
    let sample = Arc::new(Mutex::new(Sample::default()));
    let output = Arc::clone(&sample);
    let spawned = std::thread::Builder::new()
        .name("fsonos-survey-read".into())
        .spawn(move || {
            read(
                &*transport,
                &at,
                || stopping.load(Ordering::Acquire) || cancellation.load(Ordering::Acquire),
                &output,
            );
        });
    match spawned {
        Ok(thread) => model.progress.sweep.pending.push(Pending {
            target,
            stamps,
            cancelled,
            sample,
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

fn apply(model: &mut Model, pending: &mut Pending, sample: Sample, events: &mut Vec<LiveEvent>) {
    let player = &pending.target.player;
    let now = Stamps::of(&model.playback, player);
    // Transport and settings share a stamp. Only our own accepted parts may
    // advance the expected stamp; an intervening event still fences settings.
    let av_current = now.transport == pending.stamps.transport;
    if let Some((volume, mute)) = accepted(
        sample.levels,
        now.rendering == pending.stamps.rendering,
        "RenderingControl",
    ) {
        let changes = model.playback.apply_room_levels(player, volume, mute);
        record(events, player, changes);
    }
    if let Some((sample, observed)) = accepted(sample.transport, av_current, "AVTransport") {
        let changes = model.playback.apply_observation(player, &sample, observed);
        pending.stamps.transport = model
            .playback
            .observation_stamp(player, EventSource::AvTransport);
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
        record(events, player, changes);
    }
    if let Some(mode) = accepted(sample.mode, av_current, "transport settings") {
        let changes = model.playback.apply_play_mode(player, mode);
        pending.stamps.transport = model
            .playback
            .observation_stamp(player, EventSource::AvTransport);
        record(events, player, changes);
    }
}

pub(super) fn complete(engine: &Engine, model: &mut Model) {
    invalidate(model);
    if engine.stop.load(Ordering::Acquire) {
        model.progress.sweep.cancel_all();
    }
    let mut events = Vec::new();
    // Visit each of the at-most-four workers once. Returning unfinished reads
    // to the vector retains capacity and per-player exclusion, even after all
    // currently available output was consumed or cancellation was requested.
    let count = model.progress.sweep.pending.len();
    for _ in 0..count {
        let mut pending = model.progress.sweep.pending.remove(0);
        // Check before taking the mailbox: if finished, all final parts are
        // visible. A worker finishing after this check is reaped next turn.
        let finished = pending.thread.is_finished();
        let sample = std::mem::take(
            &mut *pending.sample.lock().unwrap_or_else(PoisonError::into_inner),
        );
        if !pending.cancelled.load(Ordering::Acquire) && !engine.stop.load(Ordering::Acquire) {
            apply(model, &mut pending, sample, &mut events);
        }
        if finished {
            if pending.thread.join().is_err() {
                tracing::warn!("survey read worker panicked; retaining completed services");
            }
        } else {
            model.progress.sweep.pending.push(pending);
        }
    }
    if !events.is_empty() {
        engine.publish(model, events);
    }
}

#[cfg(test)]
mod tests;
