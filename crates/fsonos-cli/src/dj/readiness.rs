//! Preview exactly the direction and library a start will use, without
//! publishing a plan, consuming a sync notice or modifying a live feed.

use super::{
    Context, DjConfig, DjSession, DjSpeakers, DjSteer, Failure, QueueFeed, SpotifyDj, State,
    directed_session, failure, plan, queued_elsewhere, speakers, steer_failure,
};
use chrono::{Datelike, Timelike};
use fsonos_core::clock::Clock;
use fsonos_core::store::Store;
use fsonos_types::PlayerId;

pub(super) fn check(
    dj: &SpotifyDj,
    at: DjSpeakers<'_>,
    store: &dyn Store,
    steer: Option<&DjSteer>,
    clock: &dyn Clock,
) -> Result<(), Failure> {
    let candidate = State::read_plan(store, dj.moods_file.as_ref(), clock.now().timestamp())?;
    let context = context(&candidate, store, at.coordinator, steer, clock)?;
    let state = dj.state();
    let elsewhere = queued_elsewhere(&state.feeds, at.coordinator, context.now);
    let fresh = QueueFeed::new(
        at.coordinator,
        DjConfig::default(),
        context.now.unsigned_abs(),
    );
    let feed = state.feeds.get(at.coordinator).unwrap_or(&fresh);
    feed.start_ready(
        &speakers(at),
        plan(
            &candidate.pool,
            candidate.feedback.as_ref(),
            &context,
            &elsewhere,
        ),
        store,
    )
    .map_err(failure)
}

fn context(
    state: &State,
    store: &dyn Store,
    coordinator: &PlayerId,
    steer: Option<&DjSteer>,
    clock: &dyn Clock,
) -> Result<Context, Failure> {
    let local = clock.now();
    let now = local.timestamp();
    let session = match steer {
        None => return state.context(store, coordinator, clock),
        Some(DjSteer::Clear) => DjSession::open(&coordinator.0),
        Some(DjSteer::Set {
            mood,
            constraints,
            for_secs,
        }) => {
            let session = directed_session(&coordinator.0, mood, constraints, *for_secs, now)?;
            // The real steer validates even a session that expires immediately.
            session.steer(&state.moods, None).map_err(|error| {
                steer_failure(error, "These constraints clash with the mood's own.")
            })?;
            session
        }
    };
    let session = if session.expired(now) {
        DjSession::open(&coordinator.0)
    } else {
        session
    };
    let steer = session
        .steer_at(
            &state.moods,
            Some(now),
            Some((local.weekday(), local.time())),
        )
        .map_err(|error| {
            steer_failure(
                error,
                "Fix the zone's DJ steering: its bounds clash with the mood's.",
            )
        })?;
    Ok(Context {
        steer,
        now,
        local_hour: u8::try_from(local.hour()).unwrap_or(0),
    })
}

#[cfg(test)]
mod tests;
