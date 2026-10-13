//! Bind a listening observation to the exact recording the feed owns.

use fsonos_spotify::feed::QueuedWork;
use fsonos_spotify::works::Completeness;

use super::{FeedbackSignal, Movement, PlayerPlayback, Signal, WorkPool, source_of};
use std::time::Instant;

pub(super) fn movement(
    queued: &[QueuedWork],
    pool: &WorkPool,
    playback: &PlayerPlayback,
    previous: Option<&Movement>,
) -> Option<Movement> {
    let position = playback.queue_position.filter(|&position| position > 0)?;
    let source = source_of(playback.track_uri.as_deref()?)?;
    let mut owners = queued.iter().filter(|work| {
        work.movements.iter().any(|movement| {
            movement.added && movement.position == Some(position) && movement.uri == source
        })
    });
    let queued = owners.next()?;
    if owners.next().is_some() {
        return None;
    }
    let mut slots = queued.movements.iter().enumerate().filter(|(_, movement)| {
        movement.added && movement.position == Some(position) && movement.uri == source
    });
    let (index, _) = slots.next()?;
    if slots.next().is_some() {
        return None;
    }
    if let Some(previous) = previous
        && previous.index == index
        && previous.work_key == queued.work_key
        && previous
            .recording
            .iter()
            .map(String::as_str)
            .eq(queued.movements.iter().map(|m| m.uri.as_str()))
    {
        // Most events describe the same movement. Retain its attribution
        // across pool reloads, without scanning the library on every event.
        let mut movement = previous.clone();
        movement.position = position;
        movement.duration_secs = playback
            .duration_secs
            .filter(|&seconds| seconds > 0)
            .or(movement.duration_secs);
        return Some(movement);
    }
    // A work key is shared across albums and performances. Match the entire
    // ordered recording, not the pool's first work with the same title/key.
    let mut candidates = pool.works().iter().filter(|work| {
        work.work_key == queued.work_key
            && work
                .movements
                .iter()
                .map(|m| m.track.source_uri.as_str())
                .eq(queued.movements.iter().map(|m| m.uri.as_str()))
    });
    let work = candidates.next()?;
    if candidates.next().is_some() {
        return None;
    }
    let duration_secs = playback.duration_secs.filter(|&seconds| seconds > 0).or(
        work.movements
            .get(index)?
            .track
            .duration_secs
            .filter(|&seconds| seconds > 0),
    );
    Some(Movement {
        position,
        work_key: work.work_key.clone(),
        last: index + 1 == work.movements.len() && work.completeness == Completeness::Complete,
        duration_secs,
        recording: queued.movements.iter().map(|m| m.uri.clone()).collect(),
        index,
        started: playback
            .position_at(Instant::now())
            .is_some_and(|seconds| seconds <= 1),
        evidence: FeedbackSignal::about(work, Signal::FullListen, 0),
    })
}
