//! Schedule admission: allocate a worker before consuming an occurrence.
//!
//! Capacity, spawn and inventory readiness failures leave the persistent
//! schedule due. Resolve its targets without writes, then check its definition
//! and enabled state under the store lock and claim it once. Never replay an
//! action after that boundary: even an error can follow a partial or
//! unacknowledged speaker mutation.

use super::{
    Ran, Schedule, ScheduleAction, Scheduler, Surface, Tick, announcements, client_of,
    core_schedule, fire, room_view, span, summary,
};
use crate::failure::{ErrorCode, Failure};
use crate::plan;
use crate::surface::scenes::scene_failure;
use chrono::{DateTime, TimeZone, Utc};
use fsonos_core::rooms::{Matched, ResolveContext, named_room, resolve_targets};
use fsonos_core::{HouseholdState, scenes};
use std::collections::HashSet;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Instant;

const MAX_RUNS: usize = 8;
type Work = Box<dyn FnOnce() + Send>;

#[derive(Default)]
pub(super) struct Dispatch {
    active: Mutex<HashSet<i64>>,
    stopping: AtomicBool,
}

impl Dispatch {
    pub(super) fn stopped(&self) -> bool {
        self.stopping.load(Ordering::Acquire)
    }

    pub(super) fn stop(&self) {
        self.stopping.store(true, Ordering::Release);
    }

    fn reserve(self: &Arc<Self>, id: i64) -> Option<Permit> {
        let mut active = self.active.lock().unwrap_or_else(PoisonError::into_inner);
        if self.stopped() || active.len() >= MAX_RUNS || !active.insert(id) {
            return None;
        }
        Some(Permit {
            dispatch: Arc::clone(self),
            id,
        })
    }
}

/// Own the reservation even if spawning fails or the worker unwinds.
struct Permit {
    dispatch: Arc<Dispatch>,
    id: i64,
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.dispatch
            .active
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.id);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Admission {
    Run,
    Skip,
    Hold,
}

pub(super) fn run_schedules<Tz: TimeZone>(
    scheduler: &Scheduler,
    now: DateTime<Utc>,
    tz: &Tz,
) -> Vec<Ran> {
    let saved = match scheduler.surface.load_schedules() {
        Ok(saved) => saved,
        Err(failure) => {
            if failure.code != ErrorCode::NotImplemented {
                tracing::warn!("schedules not read: {}", failure.detail);
            }
            return Vec::new();
        }
    };
    let mut ran = Vec::new();
    for (id, tick) in core_schedule::due(&saved, now, tz, scheduler.grace) {
        let Some(schedule) = saved.iter().find(|schedule| schedule.id == id).cloned() else {
            continue;
        };
        match tick {
            Tick::Fire { at } => {
                if launch(scheduler, schedule, at, now, |name, work| {
                    thread::Builder::new().name(name).spawn(work)
                }) {
                    ran.push(Ran::Fired { id, at });
                }
            }
            Tick::Skip { at } => {
                match claim_current(&scheduler.surface, &schedule, at, || Admission::Skip) {
                    Ok(Admission::Skip) => {
                        tracing::warn!(
                            "schedule #{id} ({}) skipped its run at {at}: more than {} late",
                            summary(&schedule.action),
                            span(scheduler.grace)
                        );
                        ran.push(Ran::Skipped { id, at });
                    }
                    Ok(Admission::Run | Admission::Hold) => {}
                    Err(failure) => {
                        tracing::warn!("schedule #{id} not recorded: {}", failure.detail);
                    }
                }
            }
        }
    }
    ran
}

/// Definition check, deadline check and durable claim share the store lock.
/// A paused/removed/resumed/edited schedule cannot run from a stale snapshot.
fn claim_current(
    surface: &Surface,
    expected: &Schedule,
    at: DateTime<Utc>,
    admit: impl FnOnce() -> Admission,
) -> Result<Admission, Failure> {
    Ok(surface
        .with_store(|store| {
            let stored = store.schedules()?;
            let current = stored
                .iter()
                .find(|stored| stored.id == expected.id)
                .and_then(|stored| Schedule::from_stored(stored).ok());
            if !current.as_ref().is_some_and(|current| {
                current == expected
                    && current.enabled
                    && current.last_fired.is_none_or(|last| last < at)
            }) {
                return Ok(Admission::Hold);
            }
            let admission = admit();
            if admission == Admission::Hold
                || !store.mark_schedule_fired(expected.id, at.timestamp())?
            {
                return Ok(Admission::Hold);
            }
            Ok(admission)
        })?
        .unwrap_or(Admission::Hold))
}

/// Readiness is checked before the durable claim, never by retrying a failed
/// action. A scheduled scene waits for all its named rooms instead of partially
/// applying while another household is still being discovered.
fn ready(surface: &Surface, schedule: &Schedule) -> Result<(), Failure> {
    let households = surface.households()?;
    if households.iter().all(|house| house.rooms.is_empty()) {
        return Err(Failure::new(ErrorCode::NotReady, "no Sonos rooms answered"));
    }
    let client = client_of(&schedule.creator);
    let aliases = surface.aliases();
    let rooms = room_view(&households, aliases.as_ref(), &client);
    match &schedule.action {
        ScheduleAction::Announce { rooms: names, .. } => {
            surface.announcement_ready()?;
            announcements::targets_ready(&rooms, names)
        }
        ScheduleAction::ApplyScene { name } => {
            let scene = surface
                .with_store(|store| Ok(scenes::load(store, name)))?
                .ok_or_else(super::no_store)?
                .map_err(scene_failure)?;
            scenes::check(&scene, &households).map_err(scene_failure)?;
            for room in scene
                .groups
                .iter()
                .flat_map(|group| std::iter::once(&group.coordinator).chain(&group.members))
                .chain(scene.volumes.keys())
                .chain(scene.mutes.keys())
            {
                named_ready(&households, room)?;
            }
            Ok(())
        }
        ScheduleAction::Pause { room, fade_secs: 0 } | ScheduleAction::Volume { room, .. } => {
            targets_ready(&rooms, room, false)
        }
        ScheduleAction::DjStart { room, .. } | ScheduleAction::Pause { room, .. } => {
            targets_ready(&rooms, room, true)
        }
    }
}

fn named_ready(households: &[HouseholdState], name: &str) -> Result<(), Failure> {
    let (_, room) = named_room(households, name)?.ok_or_else(|| {
        Failure::new(
            ErrorCode::NotReady,
            format!("{name} is not in the house yet"),
        )
    })?;
    fsonos_core::resolve_room(households, &room.primary.0)?;
    Ok(())
}

fn alias_member_ready(households: &[HouseholdState], name: &str) -> Result<(), Failure> {
    // Set aliases deliberately include all exact matches across households.
    // Preserve that fan-out while requiring every named member to be ready.
    match named_room(households, name) {
        Err(fsonos_core::CoreError::AmbiguousRoom { candidates, .. }) => {
            for candidate in candidates {
                named_ready(households, &candidate)?;
            }
            Ok(())
        }
        _ => named_ready(households, name),
    }
}

pub(super) fn targets_ready(
    rooms: &plan::Rooms<'_>,
    name: &str,
    single: bool,
) -> Result<(), Failure> {
    if single {
        plan::resolve(rooms, name)?;
    } else {
        plan::resolve_set(rooms, name)?;
    }
    let resolution = resolve_targets(
        rooms.households,
        name,
        ResolveContext {
            aliases: rooms.aliases,
            client: rooms.client,
        },
    )?;
    if let Some(aliases) = rooms.aliases {
        match resolution.matched {
            Matched::Here => {
                if let Some(default) = aliases.default_room(rooms.client) {
                    return targets_ready(rooms, default, single);
                }
            }
            Matched::Alias => {
                if let Some(alias) = aliases.get(name) {
                    for member in &alias.rooms {
                        alias_member_ready(rooms.households, member)?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn launch(
    scheduler: &Scheduler,
    schedule: Schedule,
    at: DateTime<Utc>,
    now: DateTime<Utc>,
    spawn: impl FnOnce(String, Work) -> io::Result<JoinHandle<()>>,
) -> bool {
    let Some(permit) = scheduler.dispatch.reserve(schedule.id) else {
        return false;
    };
    let surface = Arc::clone(&scheduler.surface);
    let grace = scheduler.grace;
    let admitted = Instant::now();
    let id = schedule.id;
    let work = Box::new(move || {
        let reservation = permit;
        let admission = || {
            if reservation.dispatch.stopped() {
                return Admission::Hold;
            }
            let late = (now - at)
                .to_std()
                .unwrap_or_default()
                .saturating_add(admitted.elapsed());
            if late > grace {
                Admission::Skip
            } else {
                Admission::Run
            }
        };
        if admission() == Admission::Run
            && let Err(failure) = ready(&surface, &schedule)
        {
            tracing::debug!("schedule #{id} awaits its targets: {}", failure.detail);
            return;
        }
        // A due announcement needs its normal caller slot before it can be
        // claimed. Carry the lease into delivery: checking occupancy alone
        // races immediate calls and can consume a reminder without playing it.
        let announcement = if admission() == Admission::Run
            && matches!(&schedule.action, ScheduleAction::Announce { .. })
        {
            match surface.reserve_announcement(&client_of(&schedule.creator)) {
                Ok(permit) => Some(permit),
                Err(failure) => {
                    tracing::debug!(
                        "schedule #{id} awaits announcement capacity: {}",
                        failure.detail
                    );
                    return;
                }
            }
        } else {
            None
        };
        // Readiness can take time, so recheck the grace period, shutdown and
        // current definition immediately before the irreversible claim.
        let result = claim_current(&surface, &schedule, at, admission);
        match result {
            Ok(Admission::Run) => {}
            Ok(Admission::Skip) => {
                tracing::warn!("schedule #{id} skipped: worker admission passed the grace period");
                return;
            }
            Ok(Admission::Hold) => return,
            Err(failure) => {
                tracing::warn!("schedule #{id} not claimed, so not run: {}", failure.detail);
                return;
            }
        }
        match fire(&surface, &schedule, announcement) {
            Ok(done) => tracing::info!("schedule #{id} ({}): {done}", summary(&schedule.action)),
            Err(failure) => tracing::warn!(
                "schedule #{id} ({}): {}",
                summary(&schedule.action),
                failure.cli_text()
            ),
        }
    });
    match spawn(format!("fsonos-schedule-{id}"), work) {
        Ok(handle) => {
            let mut runs = scheduler
                .runs
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            runs.retain(|run| !run.is_finished());
            runs.push(handle);
            true
        }
        Err(error) => {
            tracing::warn!("schedule #{id} worker not started; occurrence remains due: {error}");
            false
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod readiness_tests;
