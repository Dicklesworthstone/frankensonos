//! Reusable steering for scenes and scheduled starts.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::dj_direction::{DjConstraints, DjDirection};
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

use super::{DjSteer, SteerConstraints, describe_steer, span};
use crate::failure::Failure;
use crate::request::{DjStartRequest, MAX_STEER_SECS};

/// Steering reapplied before a scene or schedule starts the DJ. The mood is
/// a sibling field on the source or action. An explicit empty direction,
/// without a mood, clears a previous override and returns to automatic music.
/// Omitting the direction preserves the legacy start behavior instead.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DjDirectionDto {
    #[serde(skip_serializing_if = "SteerConstraints::is_empty")]
    pub constraints: SteerConstraints,
    /// Relative lifetime on every recall, from one second to a week.
    /// Captures retain the exact remaining lifetime, including its last minute.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub for_secs: Option<u64>,
}

impl DjDirectionDto {
    /// Check the complete direction before discovery or daemon I/O. Named
    /// moods and period vocabulary are resolved by the installed DJ engine.
    pub fn validate(&self, mood: Option<&str>) -> Result<(), Failure> {
        self.steer(mood).map(|_| ())
    }

    /// The saved selection rules and relative lifetime, in words.
    #[must_use]
    pub fn summary(&self, mood: Option<&str>) -> String {
        if mood.is_none() && self.constraints.is_empty() && self.for_secs.is_none() {
            return "automatic steering".into();
        }
        let mut text = describe_steer(mood, &self.constraints, None);
        if let Some(secs) = self.for_secs {
            let duration = if secs.is_multiple_of(60) {
                span(secs)
            } else {
                format!("{secs} second{}", if secs == 1 { "" } else { "s" })
            };
            let _ = write!(text, ", for {duration} on each start");
        }
        text
    }

    pub(crate) fn steer(&self, mood: Option<&str>) -> Result<DjSteer, Failure> {
        if let Some(secs) = self.for_secs
            && !(1..=MAX_STEER_SECS).contains(&secs)
        {
            return Err(Failure::invalid(format!(
                "direction.for_secs {secs} is outside 1..={MAX_STEER_SECS} (a second to a week)"
            )));
        }
        // Direct starts enforce a one-minute authoring minimum. Capturing a
        // live override can leave less than a minute, so validate its relative
        // lifetime above and reuse all the remaining start validation here.
        let request = DjStartRequest {
            mood: mood.map(str::to_owned),
            constraints: self.constraints.clone(),
            ..DjStartRequest::default()
        };
        match request.steer()? {
            Some(DjSteer::Set {
                mood, constraints, ..
            }) => Ok(DjSteer::Set {
                mood,
                constraints,
                for_secs: self.for_secs,
            }),
            None if self.for_secs.is_some() => Err(Failure::invalid(
                "direction.for_secs needs a mood or a constraint to steer by",
            )),
            None | Some(DjSteer::Clear) => Ok(DjSteer::Clear),
        }
    }

    pub(crate) fn normalized(&self, mood: Option<&str>) -> Result<(Option<String>, Self), Failure> {
        match self.steer(mood)? {
            DjSteer::Set {
                mood,
                constraints,
                for_secs,
            } => Ok((
                mood,
                Self {
                    constraints,
                    for_secs,
                },
            )),
            DjSteer::Clear => Ok((None, Self::default())),
        }
    }
}

impl From<&DjDirection> for DjDirectionDto {
    fn from(direction: &DjDirection) -> Self {
        Self {
            constraints: SteerConstraints::from(&direction.constraints),
            for_secs: direction.for_secs,
        }
    }
}

impl From<&DjDirectionDto> for DjDirection {
    fn from(direction: &DjDirectionDto) -> Self {
        Self {
            constraints: DjConstraints::from(&direction.constraints),
            for_secs: direction.for_secs,
        }
    }
}

impl From<&DjConstraints> for SteerConstraints {
    fn from(c: &DjConstraints) -> Self {
        Self {
            include_composers: c.include_composers.clone(),
            exclude_composers: c.exclude_composers.clone(),
            include_artists: c.include_artists.clone(),
            exclude_artists: c.exclude_artists.clone(),
            include_genres: c.include_genres.clone(),
            exclude_genres: c.exclude_genres.clone(),
            periods: c.periods.clone(),
            exclude_periods: c.exclude_periods.clone(),
            decades: c.decades.clone(),
            exclude_decades: c.exclude_decades.clone(),
            include_keywords: c.include_keywords.clone(),
            exclude_keywords: c.exclude_keywords.clone(),
            min_work_minutes: c.min_work_minutes,
            max_work_minutes: c.max_work_minutes,
            energy_bias: c.energy_bias,
            allow_long_works: c.allow_long_works,
        }
    }
}

impl From<&SteerConstraints> for DjConstraints {
    fn from(c: &SteerConstraints) -> Self {
        Self {
            include_composers: c.include_composers.clone(),
            exclude_composers: c.exclude_composers.clone(),
            include_artists: c.include_artists.clone(),
            exclude_artists: c.exclude_artists.clone(),
            include_genres: c.include_genres.clone(),
            exclude_genres: c.exclude_genres.clone(),
            periods: c.periods.clone(),
            exclude_periods: c.exclude_periods.clone(),
            decades: c.decades.clone(),
            exclude_decades: c.exclude_decades.clone(),
            include_keywords: c.include_keywords.clone(),
            exclude_keywords: c.exclude_keywords.clone(),
            min_work_minutes: c.min_work_minutes,
            max_work_minutes: c.max_work_minutes,
            energy_bias: c.energy_bias,
            allow_long_works: c.allow_long_works,
        }
    }
}

#[cfg(test)]
mod tests;
