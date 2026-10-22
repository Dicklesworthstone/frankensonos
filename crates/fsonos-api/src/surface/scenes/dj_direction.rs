//! Preserve the session's added filters, not the effective program expansion.

use fsonos_core::HouseholdState;
use fsonos_core::dj_direction::DjDirection;
use fsonos_core::scenes::{PlayingDj, Scene, SceneSource};
use fsonos_core::store::Store;

use super::store_failure;
use crate::Surface;
use crate::dj::{DjDirectionDto, DjEngine, DjSpeakers};
use crate::failure::{ErrorCode, Failure};

pub(super) fn validate(scene: &Scene) -> Result<(), Failure> {
    for group in &scene.groups {
        if let SceneSource::Dj {
            mood,
            direction: Some(direction),
        } = &group.source
        {
            DjDirectionDto::from(direction).validate(mood.as_deref())?;
        }
    }
    Ok(())
}

pub(super) fn capture(
    surface: &Surface,
    households: &[HouseholdState],
) -> Result<Vec<PlayingDj>, Failure> {
    let Some(dj) = surface.dj.as_deref() else {
        return Ok(Vec::new());
    };
    let coordinators: Vec<_> = households
        .iter()
        .flat_map(|h| &h.groups)
        .filter(|g| dj.feeds(&g.coordinator))
        .map(|g| &g.coordinator)
        .collect();
    if coordinators.is_empty() {
        return Ok(Vec::new());
    }
    // SharedStore locks per store call. Never hold the store mutex while an
    // engine method runs: the engine acquires its own lock before the store.
    surface
        .with_dj_store(|store| {
            Ok(coordinators
                .into_iter()
                .map(|coordinator| {
                    let at = DjSpeakers {
                        transport: &*surface.transport,
                        households,
                        coordinator,
                    };
                    let (mood, direction) = session(surface, dj, store, at)?;
                    Ok(PlayingDj {
                        coordinator: coordinator.clone(),
                        mood,
                        direction: Some(DjDirection::from(&direction)),
                    })
                })
                .collect())
        })?
        .unwrap_or_else(|| {
            Err(Failure::new(
                ErrorCode::NotImplemented,
                "capturing DJ direction needs the daemon's store",
            ))
        })
}

fn session(
    surface: &Surface,
    dj: &dyn DjEngine,
    store: &dyn Store,
    at: DjSpeakers<'_>,
) -> Result<(Option<String>, DjDirectionDto), Failure> {
    let before = store
        .dj_session(&at.coordinator.0)
        .map_err(|error| store_failure(&error))?;
    let now = surface.now();
    let Some(active) = before.as_ref().filter(|session| session.expires > now) else {
        return Ok((None, DjDirectionDto::default()));
    };
    // The engine parses and validates its own persisted constraints schema.
    // Failure must abort capture instead of saving an apparently automatic DJ.
    let steering = dj.status(at, store, None, &*surface.clock)?.steering;
    let after = store
        .dj_session(&at.coordinator.0)
        .map_err(|error| store_failure(&error))?;
    if before != after {
        return Err(Failure::new(
            ErrorCode::NotReady,
            "DJ direction changed while saving the scene",
        )
        .with_hint("Save the scene again after steering the DJ."));
    }
    let now = surface.now();
    if active.expires <= now {
        return Ok((None, DjDirectionDto::default()));
    }
    if steering.source != "session" {
        return Err(Failure::new(
            ErrorCode::Internal,
            "the DJ did not report its active session while saving the scene",
        ));
    }
    let for_secs = if active.expires == i64::MAX {
        None
    } else {
        Some(
            active
                .expires
                .checked_sub(now)
                .and_then(|left| u64::try_from(left).ok())
                .ok_or_else(|| {
                    Failure::invalid("the DJ session's remaining lifetime is invalid")
                })?,
        )
    };
    DjDirectionDto {
        constraints: steering.added,
        for_secs,
    }
    .normalized(active.mood.as_deref())
}
