//! Preview exactly the direction and library a start will use, without
//! publishing a plan, consuming a sync notice or modifying a live feed.

use super::{
    Context, DjConfig, DjSession, DjSpeakers, DjSteer, Failure, QueueFeed, SpotifyDj, State,
    configuration_changed, directed_session, failure, plan, plan_inputs, prefs, queued_elsewhere,
    read_moods, speakers, steer_failure,
};
use chrono::{Datelike, Timelike};
use fsonos_core::clock::Clock;
use fsonos_core::store::Store;
use fsonos_types::PlayerId;

pub(super) fn direction(
    dj: &SpotifyDj,
    at: DjSpeakers<'_>,
    store: &dyn Store,
    steer: &DjSteer,
    clock: &dyn Clock,
) -> Result<(), Failure> {
    // Clearing must remain possible while an owner repairs malformed moods
    // or preferences. The write only needs the existing session to be readable.
    if matches!(steer, DjSteer::Clear) {
        store
            .dj_session(&at.coordinator.0)
            .map_err(super::store_failure)?;
        return Ok(());
    }
    // Validating a direction only needs the owner's configuration, not every
    // track in the library. This also works before the first library sync.
    let mut candidate = State {
        moods: read_moods(dj.moods_file.as_ref())?,
        ..State::default()
    };
    prefs::apply(&mut candidate, dj.moods_file.as_ref())?;
    context(&candidate, store, at.coordinator, Some(steer), clock).map(|_| ())
}

struct StartRequest<'a> {
    steer: Option<&'a DjSteer>,
    silent: bool,
}

pub(super) fn check(
    dj: &SpotifyDj,
    at: DjSpeakers<'_>,
    store: &dyn Store,
    steer: Option<&DjSteer>,
    clock: &dyn Clock,
) -> Result<(), Failure> {
    check_for(dj, at, store, steer, clock, false)
}

pub(super) fn check_load(
    dj: &SpotifyDj,
    at: DjSpeakers<'_>,
    store: &dyn Store,
    steer: Option<&DjSteer>,
    clock: &dyn Clock,
) -> Result<(), Failure> {
    check_for(dj, at, store, steer, clock, true)
}

fn check_for(
    dj: &SpotifyDj,
    at: DjSpeakers<'_>,
    store: &dyn Store,
    steer: Option<&DjSteer>,
    clock: &dyn Clock,
    load: bool,
) -> Result<(), Failure> {
    let request = StartRequest {
        steer,
        silent: load,
    };
    {
        let state = dj.state();
        if state.plan_is_current(store, dj.moods_file.as_ref())? {
            return check_plan(&state, &state, dj, at, store, request, clock);
        }
    }
    let candidate = State::read_plan(store, dj.moods_file.as_ref(), clock.now().timestamp())?;
    let state = dj.state();
    check_plan(&candidate, &state, dj, at, store, request, clock)
}

fn check_plan(
    candidate: &State,
    state: &State,
    dj: &SpotifyDj,
    at: DjSpeakers<'_>,
    store: &dyn Store,
    request: StartRequest<'_>,
    clock: &dyn Clock,
) -> Result<(), Failure> {
    let StartRequest { steer, silent } = request;
    let context = context(candidate, store, at.coordinator, steer, clock)?;
    // Feedback and listening change without changing the catalog receipt.
    // Refresh the former here; start_ready reads current history itself.
    let feedback = super::feedback::read(store, context.now)?;
    if let Some(receipt) = &candidate.catalog {
        receipt.confirm(store)?;
    }
    if candidate.plan_inputs != Some(plan_inputs(dj.moods_file.as_ref())) {
        return Err(configuration_changed());
    }
    let elsewhere = queued_elsewhere(&state.feeds, at.coordinator, context.now);
    let fresh = QueueFeed::new(
        at.coordinator,
        DjConfig::default(),
        context.now.unsigned_abs(),
    );
    let feed = state.feeds.get(at.coordinator).unwrap_or(&fresh);
    let planning = plan(
        &candidate.pool,
        candidate.catalog.as_ref(),
        Some(&feedback),
        &context,
        &elsewhere,
    );
    if silent {
        feed.load_ready(&speakers(at), planning, store)
    } else {
        feed.start_ready(&speakers(at), planning, store)
    }
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
