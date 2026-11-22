//! Bedtime controls keep one admitted room set and one action-history boundary.
//! Every group keeps its own timer/fader; failures never replay the request.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::HouseholdState;
use fsonos_core::policy::Client;
use fsonos_types::{PlayerId, TransportState};
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use std::thread;
use std::time::Duration;

use super::{
    FadeOutcome, Fader, SleepRequest, SleepTimerDto, Surface, Zone, room_view, soap, span,
};
use crate::plan::{Command, Rooms, TransportAction};
use crate::request::zone_name;
use crate::surface::audience::AudienceTransport;
use crate::surface::players::targets::resolve_view;
use crate::surface::{set_runs, source_control};
use crate::{ApiError, ErrorCode, Failure, OutcomeDto};

/// One group retains its existing response; a set accounts for every group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SleepOutcome {
    Single(SleepTimerDto),
    Groups(SleepSetDto),
}

impl fastapi_openapi::JsonSchema for SleepOutcome {
    fn schema() -> fastapi_openapi::Schema {
        fastapi_openapi::Schema::one_of(vec![
            <SleepTimerDto as fastapi_openapi::JsonSchema>::schema(),
            <SleepSetDto as fastapi_openapi::JsonSchema>::schema(),
        ])
    }

    fn schema_name() -> Option<&'static str> {
        Some("SleepOutcome")
    }
}

impl SleepOutcome {
    #[must_use]
    pub fn done(&self) -> &str {
        match self {
            Self::Single(timer) => &timer.done,
            Self::Groups(batch) => &batch.done,
        }
    }

    #[must_use]
    pub fn complete(&self) -> bool {
        match self {
            Self::Single(_) => true,
            Self::Groups(batch) => {
                batch.complete && batch.failed.is_empty() && !batch.applied.is_empty()
            }
        }
    }

    /// Keep per-group results in human output as well as structured receipts.
    #[must_use]
    pub fn text(&self) -> String {
        let mut text = format!("{}\n", self.done());
        if let Self::Groups(batch) = self {
            for applied in &batch.applied {
                let _ = writeln!(
                    text,
                    "{}@{}: {}",
                    applied.coordinator, applied.household, applied.timer.done
                );
            }
            for failed in &batch.failed {
                let _ = writeln!(
                    text,
                    "{}@{} ({}): {} ({}). {}",
                    failed.coordinator,
                    failed.household,
                    failed.zone,
                    failed.error.detail,
                    failed.error.code.as_str(),
                    failed.error.hint
                );
            }
            if !self.complete() {
                text.push_str("Inspect the affected timers before another action; do not replay the whole request.\n");
            }
        }
        text
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SleepSetDto {
    pub done: String,
    pub complete: bool,
    /// At least one group confirmed a timer change. Failures can have uncertain effects.
    pub changed: bool,
    pub applied: Vec<SleepAppliedDto>,
    pub failed: Vec<SleepTargetFailureDto>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SleepAppliedDto {
    pub household: String,
    pub coordinator: String,
    pub timer: SleepTimerDto,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SleepTargetFailureDto {
    pub household: String,
    pub coordinator: String,
    pub zone: String,
    pub error: ApiError,
}

#[derive(Debug)]
pub(super) struct BedtimeGroup {
    pub(super) zone: Zone,
    household: String,
}

/// Scheduling carries this snapshot across its durable occurrence claim.
#[derive(Debug)]
pub(super) struct PreparedBedtime {
    households: Vec<HouseholdState>,
    groups: Vec<BedtimeGroup>,
    target: String,
}

pub(super) fn prepare(
    rooms: &Rooms<'_>,
    target: &str,
    revision: Option<&str>,
) -> Result<PreparedBedtime, Failure> {
    let target = zone_name("zone", target)?;
    if let Some(revision) = revision {
        source_control::validate_revision(revision)?;
    }
    let resolved = resolve_view(rooms, target)?;
    if revision.is_some_and(|revision| revision != resolved.revision) {
        return Err(Failure::new(
            ErrorCode::NotCoordinator,
            "the reviewed bedtime target or its physical groups changed; nothing was changed",
        )
        .with_hint(
            "Read the target and review every affected group before a new bedtime command.",
        ));
    }
    let groups = resolved
        .groups
        .into_iter()
        .map(|group| {
            Zone::of(rooms.households, &PlayerId(group.coordinator))
                .map(|zone| BedtimeGroup {
                    zone,
                    household: group.household,
                })
                .ok_or_else(|| {
                    Failure::new(
                        ErrorCode::NotReady,
                        "a selected bedtime coordinator is unavailable",
                    )
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if groups.is_empty() {
        return Err(Failure::new(
            ErrorCode::NotReady,
            "no groups are available for this bedtime target",
        ));
    }
    Ok(PreparedBedtime {
        households: rooms.households.to_vec(),
        groups,
        target: target.into(),
    })
}

/// Bounded per-group work; a failed thread allocation runs that group once here.
fn together<I: Sync, T: Send>(items: &[I], work: impl Fn(&I) -> T + Sync) -> Vec<T> {
    let mut results = Vec::with_capacity(items.len());
    let work = &work;
    for group in items.chunks(8) {
        results.extend(thread::scope(|scope| {
            let jobs: Vec<_> = group
                .iter()
                .map(|item| {
                    thread::Builder::new()
                        .name("fsonos-bedtime".into())
                        .stack_size(8 * 1024 * 1024)
                        .spawn_scoped(scope, move || work(item))
                        .map_err(|_| work(item))
                })
                .collect();
            jobs.into_iter()
                .map(|job| match job {
                    Ok(job) => job
                        .join()
                        .unwrap_or_else(|panic| std::panic::resume_unwind(panic)),
                    Err(result) => result,
                })
                .collect::<Vec<_>>()
        }));
    }
    results
}

fn possible_change<T>(result: &Result<T, Failure>, changed: impl FnOnce(&T) -> bool) -> bool {
    match result {
        Ok(value) => changed(value),
        Err(failure) => failure.code == ErrorCode::OutcomeUncertain,
    }
}

impl Surface {
    pub(super) fn bedtime_targets(
        &self,
        client: &Client,
        target: &str,
        revision: Option<&str>,
    ) -> Result<(Vec<HouseholdState>, Vec<BedtimeGroup>), Failure> {
        let households = self.households()?;
        let aliases = self.aliases();
        let prepared = prepare(
            &room_view(&households, aliases.as_ref(), client),
            target,
            revision,
        )
        .map_err(|failure| self.explain(failure))?;
        Ok((prepared.households, prepared.groups))
    }

    /// Set, extend or cancel every distinct selected group's existing timer.
    pub fn set_sleep_timers(
        &self,
        client: &Client,
        req: &SleepRequest,
    ) -> Result<SleepOutcome, Failure> {
        self.change_sleep_timers(client, req, false)
    }

    pub(super) fn change_sleep_timers(
        &self,
        client: &Client,
        req: &SleepRequest,
        single: bool,
    ) -> Result<SleepOutcome, Failure> {
        self.authorize_write(client, "set_sleep_timer")?;
        let change = req.change()?;
        let (households, groups) =
            self.bedtime_targets(client, req.zone()?, req.expected_revision.as_deref())?;
        if single && groups.len() != 1 {
            return Err(Failure::invalid(
                "set_sleep_timer requires one group; use set_sleep_timers for a room set",
            ));
        }
        if groups.len() == 1 && req.expected_revision.is_none() {
            let mut pinned = req.clone();
            pinned.zone.clone_from(&groups[0].zone.coordinator.0);
            return self
                .set_sleep_timer(client, &pinned)
                .map(SleepOutcome::Single);
        }
        let now = self.clock.now();
        let results = together(&groups, |group| {
            let audience = AudienceTransport::new(self, &households, &group.zone.coordinator);
            let result = (|| {
                self.guard(client).authorize("set_sleep_timer", false)?;
                audience.check()?;
                match &self.sleep {
                    Some(sleep) => Self::sleep_in_daemon(
                        &audience,
                        sleep,
                        &households,
                        &group.zone,
                        change,
                        now,
                    ),
                    None => Self::sleep_on_speaker(&audience, &group.zone, change, now),
                }
            })();
            audience.finish(result)
        });
        if let Some(sleep) = &self.sleep {
            sleep.save();
        }
        let kept = results
            .iter()
            .any(|result| possible_change(result, |timer| timer.changed));
        let mut applied = Vec::new();
        let mut failed = Vec::new();
        let single_failure = (groups.len() == 1)
            .then(|| results[0].as_ref().err().cloned())
            .flatten();
        for (group, result) in groups.into_iter().zip(results) {
            match result {
                Ok(timer) => applied.push(SleepAppliedDto {
                    household: group.household,
                    coordinator: group.zone.coordinator.0,
                    timer,
                }),
                Err(failure) => {
                    self.notice(&failure);
                    failed.push(SleepTargetFailureDto {
                        household: group.household,
                        coordinator: group.zone.coordinator.0,
                        zone: group.zone.room,
                        error: failure.into(),
                    });
                }
            }
        }
        let result = if let Some(failure) = single_failure {
            Err(failure)
        } else if applied.len() == 1 && failed.is_empty() {
            Ok(SleepOutcome::Single(applied.remove(0).timer))
        } else {
            let complete = failed.is_empty();
            let done = format!(
                "sleep timer change {change}: {} group(s) confirmed{}",
                applied.len(),
                if complete {
                    String::new()
                } else {
                    format!("; incomplete for {} group(s)", failed.len())
                }
            );
            Ok(SleepOutcome::Groups(SleepSetDto {
                done,
                complete,
                changed: applied.iter().any(|applied| applied.timer.changed),
                applied,
                failed,
            }))
        };
        self.log_kept_write(
            client,
            format!("set_sleep_timer: {} {change}", req.zone.trim()),
            &result,
            SleepOutcome::text,
            kept.then_some("sleep timer"),
        );
        result
    }

    pub(super) fn bedtime_ready(&self, prepared: &PreparedBedtime) -> Result<(), Failure> {
        for group in &prepared.groups {
            self.confirm_reviewed_group(&prepared.households, &group.zone.coordinator)?;
            if soap::get_transport_info(&*self.transport, group.zone.ip)
                .map_err(super::proto)?
                .state
                == TransportState::Unknown
            {
                return Err(Failure::new(
                    ErrorCode::NotReady,
                    "a bedtime group's playback state is unknown",
                ));
            }
        }
        Ok(())
    }

    pub(super) fn fade_bedtime_groups(
        &self,
        client: &Client,
        tool: &str,
        target: &str,
        over: Duration,
        revision: Option<&str>,
    ) -> Result<OutcomeDto, Failure> {
        if let Some(revision) = revision {
            source_control::validate_revision(revision)?;
        }
        self.authorize_write(client, tool)?;
        let households = self.households()?;
        let aliases = self.aliases();
        let prepared = prepare(
            &room_view(&households, aliases.as_ref(), client),
            target,
            revision,
        )
        .map_err(|failure| self.explain(failure))?;
        if prepared.groups.len() == 1 && revision.is_none() {
            return self.fade_one_group(client, tool, &prepared.groups[0].zone.coordinator.0, over);
        }
        self.fade_prepared_groups(client, tool, prepared, over)
    }

    pub(super) fn fade_prepared_groups(
        &self,
        client: &Client,
        tool: &str,
        prepared: PreparedBedtime,
        over: Duration,
    ) -> Result<OutcomeDto, Failure> {
        self.authorize_write(client, tool)?;
        let own = Fader::default();
        let fader = self.sleep.as_ref().map_or(&own, |sleep| &sleep.fader);
        let results = together(&prepared.groups, |group| {
            let audience =
                AudienceTransport::new(self, &prepared.households, &group.zone.coordinator);
            let result = (|| {
                self.guard(client).authorize(tool, false)?;
                audience.check()?;
                let state = soap::get_transport_info(&audience, group.zone.ip)
                    .map_err(super::proto)?
                    .state;
                if matches!(state, TransportState::Paused | TransportState::Stopped) {
                    return Ok(OutcomeDto {
                        done: format!(
                            "{}'s group is already inactive; no fade was sent",
                            group.zone.room
                        ),
                        changed: false,
                        volume: None,
                        notes: Vec::new(),
                        batch: None,
                    });
                }
                if state == TransportState::Unknown {
                    return Err(Failure::new(
                        ErrorCode::NotReady,
                        "the group's playback state is unknown; no fade was sent",
                    ));
                }
                let outcome = fader.fade_out_and_pause(
                    &audience,
                    &prepared.households,
                    &group.zone.coordinator,
                    over,
                )?;
                Ok(OutcomeDto {
                    done: match outcome {
                        FadeOutcome::Reached => format!(
                            "faded {}'s group out over {} and paused it",
                            group.zone.room,
                            span(over)
                        ),
                        FadeOutcome::Superseded { .. } => format!(
                            "stopped fading {}'s group: its volume was changed meanwhile",
                            group.zone.room
                        ),
                    },
                    changed: true,
                    volume: None,
                    notes: Vec::new(),
                    batch: None,
                })
            })();
            audience.finish(result)
        });
        let kept = results
            .iter()
            .any(|result| possible_change(result, |outcome| outcome.changed));
        let commands: Vec<_> = prepared
            .groups
            .iter()
            .map(|group| Command::Transport {
                member: group.zone.coordinator.clone(),
                coordinator: group.zone.coordinator.clone(),
                action: TransportAction::Pause,
            })
            .collect();
        let result = set_runs::merged(&prepared.households, &commands, results);
        self.log_kept_write(
            client,
            format!("pause: {} after a {} fade", prepared.target, span(over)),
            &result,
            |outcome| {
                let mut text = outcome.done.clone();
                for note in &outcome.notes {
                    let _ = write!(text, "; {}", note.detail);
                }
                text
            },
            kept.then_some("fade-out"),
        );
        result
    }
}

#[cfg(test)]
mod tests;
