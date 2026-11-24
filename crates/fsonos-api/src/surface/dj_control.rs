//! DJ controls bound to the audience a caller reviewed with `/rooms/resolve`.
//! The observation is checked before readiness, then the shared executor
//! retains those physical groups through engine waits and speaker writes.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::policy::Client;
use serde::{Deserialize, Serialize};

use super::Surface;
use super::players::targets::resolve_view;
use crate::OutcomeDto;
use crate::dj::SteerConstraints;
use crate::failure::{ErrorCode, Failure};
use crate::plan::{self, Command, DjAction, Rooms};
use crate::request::{DjStartRequest, DjSteerRequest, ZoneRequest, zone_name};

/// The existing DJ actions, with the same policy and action-log names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewedDjAction {
    Start,
    Steer,
    Skip,
    Stop,
}

impl fastapi_openapi::JsonSchema for ReviewedDjAction {
    fn schema() -> fastapi_openapi::Schema {
        fastapi_openapi::Schema::string_enum(
            ["start", "steer", "skip", "stop"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        )
    }

    fn schema_name() -> Option<&'static str> {
        Some("ReviewedDjAction")
    }
}

impl ReviewedDjAction {
    const fn tool(self) -> &'static str {
        match self {
            Self::Start => "dj_start",
            Self::Steer => "dj_steer",
            Self::Skip => "dj_skip",
            Self::Stop => "dj_stop",
        }
    }
}

/// `POST /dj/control`: one reviewed operation over independent groups.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReviewedDjRequest {
    /// A room, alias, `here`, `all` or household-qualified target.
    pub zone: String,
    /// The revision from the latest review of this same target.
    pub expected_revision: String,
    pub action: ReviewedDjAction,
    #[serde(default)]
    pub mood: Option<String>,
    #[serde(default)]
    pub constraints: SteerConstraints,
    #[serde(default)]
    pub for_secs: Option<u64>,
    /// Clear each group's saved direction; only valid for `steer`.
    #[serde(default)]
    pub clear: bool,
}

impl ReviewedDjRequest {
    /// Invalid action fields fail before discovery or readiness reads.
    pub fn validate(&self) -> Result<(), Failure> {
        zone_name("zone", &self.zone)?;
        if self.expected_revision.len() != 64
            || !self
                .expected_revision
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(Failure::invalid(
                "expected_revision must be the revision returned by /rooms/resolve for this target",
            ));
        }
        match self.action {
            ReviewedDjAction::Start => {
                if self.clear {
                    return Err(Failure::invalid(
                        "clear is only valid for a DJ steer action",
                    ));
                }
                self.start().steer()?;
            }
            ReviewedDjAction::Steer => {
                self.steer().steer()?;
            }
            ReviewedDjAction::Skip | ReviewedDjAction::Stop => {
                if self.mood.is_some()
                    || !self.constraints.is_empty()
                    || self.for_secs.is_some()
                    || self.clear
                {
                    return Err(Failure::invalid(
                        "DJ skip and stop do not accept a mood, constraints, duration or clear",
                    ));
                }
            }
        }
        Ok(())
    }

    fn start(&self) -> DjStartRequest {
        DjStartRequest {
            zone: self.zone.clone(),
            mood: self.mood.clone(),
            constraints: self.constraints.clone(),
            for_secs: self.for_secs,
        }
    }

    fn steer(&self) -> DjSteerRequest {
        DjSteerRequest {
            zone: self.zone.clone(),
            mood: self.mood.clone(),
            constraints: self.constraints.clone(),
            for_secs: self.for_secs,
            clear: self.clear,
        }
    }

    fn plan(&self, rooms: &Rooms<'_>) -> Result<Vec<Command>, Failure> {
        let target = zone_name("zone", &self.zone)?;
        if resolve_view(rooms, target)?.revision != self.expected_revision {
            return Err(Failure::new(
                ErrorCode::NotCoordinator,
                "the reviewed DJ target or its physical groups changed; nothing was changed",
            )
            .with_hint("Read the target again and review every affected group before submitting a new DJ action."));
        }
        match self.action {
            ReviewedDjAction::Start => plan::plan_dj_start_set(rooms, &self.start()),
            ReviewedDjAction::Steer => plan::plan_dj_steer_set(rooms, &self.steer()),
            ReviewedDjAction::Skip | ReviewedDjAction::Stop => plan::plan_dj_set(
                rooms,
                &ZoneRequest {
                    zone: target.into(),
                },
                if self.action == ReviewedDjAction::Skip {
                    DjAction::Skip
                } else {
                    DjAction::Stop
                },
            ),
        }
    }
}

impl Surface {
    /// Apply a reviewed DJ request once, preserving its admitted audience.
    pub fn control_reviewed_dj(
        &self,
        client: &Client,
        request: &ReviewedDjRequest,
    ) -> Result<OutcomeDto, Failure> {
        request.validate()?;
        let tool = request.action.tool();
        let prepared = self.prepare_dj_controls(client, tool, |rooms| request.plan(rooms))?;
        self.control_prepared_dj(client, tool, prepared)
    }
}

#[cfg(test)]
mod tests;
