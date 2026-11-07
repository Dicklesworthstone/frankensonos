//! Validate the exact prospective direction before a scene changes the house.

use fsonos_core::HouseholdState;
use fsonos_core::dj_direction::DjDirection;
use fsonos_core::scenes::{SceneOp, ScenePlan};

use crate::Surface;
use crate::dj::{DjDirectionDto, DjSpeakers, DjSteer, SteerConstraints};
use crate::failure::{ErrorCode, Failure};

pub(super) fn steering(
    surface: &Surface,
    mood: Option<&str>,
    direction: Option<&DjDirection>,
) -> Result<Option<DjSteer>, Failure> {
    match direction {
        Some(direction) => DjDirectionDto::from(direction).steer(mood).map(Some),
        // Legacy mood-only scenes last until the program takes over. An
        // omitted direction and mood leave the existing steering in effect.
        None => Ok(mood.map(|mood| DjSteer::Set {
            mood: Some(mood.to_lowercase()),
            constraints: SteerConstraints::default(),
            for_secs: surface.mood_lasts(),
        })),
    }
}

pub(super) fn check_readiness(
    surface: &Surface,
    households: &[HouseholdState],
    plan: &ScenePlan,
) -> Result<(), Failure> {
    for op in &plan.ops {
        let SceneOp::Dj {
            coordinator,
            mood,
            direction,
        } = op
        else {
            continue;
        };
        let dj = surface
            .dj
            .as_deref()
            .ok_or_else(|| Failure::new(ErrorCode::NotImplemented, "this surface runs no DJ"))?;
        let steer = steering(surface, mood.as_deref(), direction.as_ref())?;
        let at = DjSpeakers {
            transport: &*surface.transport,
            households,
            // A scene can elect a different coordinator. Readiness addresses
            // that player in the current inventory without regrouping first.
            coordinator,
        };
        surface
            .with_dj_store(|store| Ok(dj.start_ready(at, store, steer.as_ref(), &*surface.clock)))?
            .ok_or_else(|| {
                Failure::new(ErrorCode::NotImplemented, "the DJ needs the daemon's store")
            })??;
    }
    Ok(())
}
