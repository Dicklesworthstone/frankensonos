//! Edit one observed schedule without replacing its identity or run principal.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::store::StoredSchedule;
use serde::{Deserialize, Serialize};

use super::{
    Client, ErrorCode, Failure, Schedule, ScheduleDto, ScheduleRequest, ScheduleSpec, Surface,
    client_of, no_store, schedule_failure,
};

/// Replace the editable definition of an existing schedule at its observed
/// revision. Identity, creator, creation time and enabled state are retained.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateScheduleRequest {
    pub id: i64,
    /// The exact revision returned by list_schedules or a schedule mutation.
    pub expected_revision: String,
    /// A complete replacement definition, using the same fields as add_schedule.
    pub schedule: ScheduleRequest,
}

impl UpdateScheduleRequest {
    /// Check the selection and action before daemon discovery or any store I/O.
    /// Relative times are interpreted by the selected daemon's clock.
    pub fn validate(&self) -> Result<(), Failure> {
        if self.id <= 0 {
            return Err(Failure::invalid("schedule id must be positive"));
        }
        if self.expected_revision.trim().is_empty()
            || self.expected_revision.len() > 4096
            || self.expected_revision.chars().any(char::is_control)
        {
            return Err(Failure::invalid(
                "editing a schedule requires its observed revision",
            ));
        }
        if self.schedule.when.trim().is_empty()
            || self.schedule.when.len() > 1024
            || self.schedule.when.chars().any(char::is_control)
        {
            return Err(Failure::invalid(
                "give a schedule time without control characters",
            ));
        }
        self.schedule.validate()
    }
}

pub(super) fn changed() -> Failure {
    Failure::new(
        ErrorCode::ConfigChanged,
        "the schedule changed after it was read",
    )
    .with_hint("Read the current schedules and review the edit again. No edit was retried.")
}

impl Surface {
    /// Read the exact persisted row alongside the validated schedule. CAS must
    /// retain legacy serialized bytes, rather than re-encoding the expected row.
    pub(super) fn observed_schedule(&self, id: i64) -> Result<(Schedule, StoredSchedule), Failure> {
        let rows = self
            .with_store(|store| store.schedules())?
            .ok_or_else(no_store)?;
        let ids = rows
            .iter()
            .map(|row| row.id.to_string())
            .collect::<Vec<_>>();
        let row = rows.into_iter().find(|row| row.id == id).ok_or_else(|| {
            Failure::new(ErrorCode::UnknownSchedule, format!("no schedule #{id}"))
                .with_suggestions(ids)
        })?;
        let schedule = Schedule::from_stored(&row).map_err(|error| schedule_failure(&error))?;
        Ok((schedule, row))
    }

    /// Change future runs of one reviewed schedule. An already claimed run may
    /// finish under its old definition; its claim makes this revision stale.
    pub fn update_schedule(
        &self,
        client: &Client,
        req: &UpdateScheduleRequest,
    ) -> Result<ScheduleDto, Failure> {
        self.authorize_write(client, "update_schedule")?;
        req.validate()?;
        let _configuration = self
            .dj_configuration_gate
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _restoring = self.log.as_ref().map(|log| {
            log.restoring
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        let (observed, row) = self.observed_schedule(req.id)?;
        self.authorize_change(
            client,
            "update_schedule",
            &format!("#{}", req.id),
            Some(&observed.creator),
        )?;
        if observed.revision() != req.expected_revision {
            return Err(changed());
        }
        let action = req.schedule.action()?;
        self.validate_schedule_action(client, &action)?;
        let creator = client_of(&observed.creator);
        if creator.key() != client.key() {
            self.validate_schedule_action(&creator, &action)?;
        }
        let now = self.clock.now();
        // Returning an unchanged, completed one-shot is a valid no-op. Any
        // changed one-shot must instead choose a future occurrence explicitly.
        let same_observed = ScheduleSpec::from_canonical(&req.schedule.when.to_lowercase()).ok()
            == Some(observed.spec)
            && action == observed.action;
        let spec = if same_observed {
            observed.spec
        } else {
            ScheduleSpec::parse(&req.schedule.when, now)
                .map_err(|error| schedule_failure(&error))?
        };
        let unchanged = spec == observed.spec && action == observed.action;
        let mut replacement = row.clone();
        if !unchanged {
            replacement.spec = spec.to_string();
            replacement.action = serde_json::to_string(&action)
                .map_err(|error| Failure::new(ErrorCode::Internal, error.to_string()))?;
            replacement.last_fired = row.last_fired.max(Some(now.timestamp()));
            if let ScheduleSpec::Once(at) = spec
                && Some(at.timestamp()) <= replacement.last_fired
            {
                return Err(Failure::invalid(
                    "choose a one-shot time after the schedule's last consumed occurrence",
                ));
            }
        }
        let result = self
            .with_store(|store| store.replace_schedule(&row, &replacement))?
            .ok_or_else(no_store)
            .and_then(|replaced| if replaced { Ok(()) } else { Err(changed()) })
            .and_then(|()| {
                Schedule::from_stored(&replacement).map_err(|error| schedule_failure(&error))
            })
            .map(|schedule| ScheduleDto::new(&schedule, now.to_utc()));
        self.log_kept_write(
            client,
            format!("update_schedule: #{}", req.id),
            &result,
            |schedule| {
                format!(
                    "{} {}",
                    if unchanged { "unchanged" } else { "updated" },
                    schedule.line()
                )
            },
            (result.is_ok() && !unchanged).then_some("schedule edit"),
        );
        result
    }
}

#[cfg(test)]
mod tests;
