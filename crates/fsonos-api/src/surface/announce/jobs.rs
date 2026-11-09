//! Bounded, process-local announcement jobs. Cancellation requests release
//! waiting clips or let the core finish ownership-checked restoration.

use super::{AnnounceDto, AnnounceRequest, Surface};
use crate::{ApiError, ErrorCode, Failure};
use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::announce::{AnnouncementControl, AnnouncementPhase};
use fsonos_core::policy::Client;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

/// Includes active jobs and the newest terminal receipts. Restarting the
/// daemon retires these IDs; no recording or speech text is retained here.
pub const MAX_RETAINED: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AnnouncementJobPhase {
    Queued,
    Preparing,
    Playing,
    Restoring,
    Completed,
    Cancelled,
    Failed,
}

impl AnnouncementJobPhase {
    const fn terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled | Self::Failed)
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Preparing => "preparing",
            Self::Playing => "playing",
            Self::Restoring => "restoring",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }
}

/// Independent household progress; one household can wait while another plays.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AnnouncementProgressDto {
    pub household: Option<String>,
    pub lead: String,
    pub rooms: Vec<String>,
    /// queued, preparing, playing, restoring, or complete.
    pub phase: String,
}

/// A current observation, not a claim that a cancellation has finished.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AnnouncementJobDto {
    pub id: String,
    pub done: String,
    /// speech, chime, or wav. The spoken words and uploaded bytes are private
    /// transient input, never copied into the job listing.
    pub kind: String,
    pub rooms: Vec<String>,
    pub phase: AnnouncementJobPhase,
    pub created_at: i64,
    pub finished_at: Option<i64>,
    pub cancel_requested: bool,
    pub can_cancel: bool,
    pub progress: Vec<AnnouncementProgressDto>,
    pub result: Option<AnnounceDto>,
    pub error: Option<ApiError>,
}

impl AnnouncementJobDto {
    #[must_use]
    pub fn text(&self) -> String {
        format!("{}\n", self.done)
    }
}

struct State {
    phase: AnnouncementJobPhase,
    finished_at: Option<i64>,
    result: Option<AnnounceDto>,
    error: Option<ApiError>,
}

pub(super) struct Job {
    id: String,
    owner: String,
    kind: String,
    rooms: Vec<String>,
    created_at: i64,
    pub(super) control: AnnouncementControl,
    state: Mutex<State>,
}

impl Job {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn preparing(&self) {
        self.state().phase = AnnouncementJobPhase::Preparing;
    }

    pub(super) fn finish(&self, result: &Result<AnnounceDto, Failure>, at: i64) {
        let mut state = self.state();
        if state.finished_at.is_some() {
            return;
        }
        state.finished_at = Some(at);
        match result {
            Ok(report) => {
                let cancelled = report.households.iter().any(|h| h.outcome == "cancelled");
                let restored = report.households.iter().all(|h| {
                    h.error.is_none()
                        && h.not_restored.is_empty()
                        && matches!(h.outcome.as_str(), "finished" | "cancelled")
                });
                state.phase = if cancelled && restored {
                    AnnouncementJobPhase::Cancelled
                } else if report.clean {
                    AnnouncementJobPhase::Completed
                } else {
                    AnnouncementJobPhase::Failed
                };
                state.result = Some(report.clone());
            }
            Err(error) => {
                state.phase = AnnouncementJobPhase::Failed;
                state.error = Some(error.clone().into());
            }
        }
    }

    fn view(&self) -> AnnouncementJobDto {
        let state = self.state();
        let progress: Vec<_> = self
            .control
            .progress()
            .into_iter()
            .map(|row| AnnouncementProgressDto {
                household: row.household.map(|household| household.0),
                lead: row.lead.0,
                rooms: row.rooms,
                phase: match row.phase {
                    AnnouncementPhase::Queued => "queued",
                    AnnouncementPhase::Preparing => "preparing",
                    AnnouncementPhase::Playing => "playing",
                    AnnouncementPhase::Restoring => "restoring",
                    AnnouncementPhase::Complete => "complete",
                }
                .into(),
            })
            .collect();
        let phase = if state.phase.terminal() || progress.is_empty() {
            state.phase
        } else if progress.iter().any(|row| row.phase == "playing") {
            AnnouncementJobPhase::Playing
        } else if progress.iter().any(|row| row.phase == "restoring") {
            AnnouncementJobPhase::Restoring
        } else if progress.iter().any(|row| row.phase == "preparing") {
            AnnouncementJobPhase::Preparing
        } else if progress.iter().any(|row| row.phase == "queued") {
            AnnouncementJobPhase::Queued
        } else {
            // The final report and sleep recovery still belong to delivery.
            AnnouncementJobPhase::Restoring
        };
        let cancel_requested = self.control.is_cancelled();
        let mut done = format!("announcement {}: {}", self.id, phase.name());
        if cancel_requested && !phase.terminal() {
            done.push_str("; cancellation requested; wait for playback and restoration to finish");
        }
        if let Some(report) = &state.result {
            done.push_str("; ");
            done.push_str(&report.done);
        }
        if let Some(error) = &state.error {
            done.push_str("; ");
            done.push_str(&error.detail);
        }
        AnnouncementJobDto {
            id: self.id.clone(),
            done,
            kind: self.kind.clone(),
            rooms: self.rooms.clone(),
            phase,
            created_at: self.created_at,
            finished_at: state.finished_at,
            cancel_requested,
            can_cancel: !phase.terminal() && !cancel_requested,
            progress,
            result: state.result.clone(),
            error: state.error.clone(),
        }
    }

    fn cancel(&self) -> (bool, AnnouncementJobDto) {
        // Serialize the final terminal publication with cancellation admission.
        // A late cancel is a read of the completed receipt, never a new Stop.
        let changed = {
            let state = self.state();
            !state.phase.terminal() && self.control.cancel()
        };
        (changed, self.view())
    }
}

#[derive(Default)]
pub(super) struct Jobs {
    entries: Mutex<VecDeque<Arc<Job>>>,
}

impl Jobs {
    fn entries(&self) -> std::sync::MutexGuard<'_, VecDeque<Arc<Job>>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn insert(
        &self,
        client: &Client,
        kind: &str,
        rooms: Vec<String>,
        at: i64,
    ) -> Result<Arc<Job>, Failure> {
        if rooms.is_empty() || rooms.len() > 512 {
            return Err(Failure::invalid(
                "an announcement needs between 1 and 512 resolved rooms",
            ));
        }
        let id = fsonos_core::announce::clip::clip_id().map_err(|error| {
            Failure::new(
                ErrorCode::Internal,
                format!("cannot create announcement identity: {error}"),
            )
        })?;
        let job = Arc::new(Job {
            id,
            owner: client.key().into(),
            kind: kind.into(),
            rooms,
            created_at: at,
            control: AnnouncementControl::default(),
            state: Mutex::new(State {
                phase: AnnouncementJobPhase::Queued,
                finished_at: None,
                result: None,
                error: None,
            }),
        });
        let mut entries = self.entries();
        while entries.len() >= MAX_RETAINED {
            let Some(oldest) = entries
                .iter()
                .position(|entry| entry.state().phase.terminal())
            else {
                return Err(Failure::new(
                    ErrorCode::NotReady,
                    "announcement receipt capacity is full",
                ));
            };
            entries.remove(oldest);
        }
        entries.push_back(Arc::clone(&job));
        Ok(job)
    }

    fn visible(&self, client: &Client, owner: bool) -> Vec<Arc<Job>> {
        self.entries()
            .iter()
            .rev()
            .filter(|entry| owner || entry.owner == client.key())
            .cloned()
            .collect()
    }
}

pub fn validate_id(id: &str) -> Result<(), Failure> {
    if id.len() != 32
        || !id
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(Failure::invalid(
            "announcement id must be the 32 lowercase hex digits returned by the daemon",
        ));
    }
    Ok(())
}

impl Surface {
    /// Start once in the background; the returned ID can be inspected after a
    /// browser disconnect. Admission uses the same per-client/global lease as
    /// synchronous and scheduled announcements, including during synthesis.
    pub fn start_announcement(
        self: &Arc<Self>,
        client: &Client,
        req: &AnnounceRequest,
    ) -> Result<AnnouncementJobDto, Failure> {
        let (prepared, turn, job) =
            self.prepare_announcement(client, req, None, "announce_start")?;
        let admitted = job.view();
        let surface = Arc::clone(self);
        let caller = client.clone();
        let running = Arc::clone(&job);
        let started = std::thread::Builder::new().name("fsonos-announcement".into()).spawn(move || {
            let _turn = turn;
            running.preparing();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                surface.deliver_announcement(&caller, prepared, &running.control)
            })).unwrap_or_else(|_| Err(Failure::new(ErrorCode::Internal,
                "announcement worker stopped unexpectedly; inspect playback before another command")));
            running.finish(&result, surface.clock.now().timestamp());
        });
        if let Err(error) = started {
            let failure = Failure::new(
                ErrorCode::Internal,
                format!("cannot start announcement worker: {error}"),
            );
            job.finish(&Err(failure.clone()), self.clock.now().timestamp());
            return Err(failure);
        }
        Ok(admitted)
    }

    fn announcement_owner(&self, client: &Client) -> bool {
        match client {
            Client::Cli => true,
            Client::Tailnet(login) => self
                .policy()
                .owner()
                .is_some_and(|owner| owner.eq_ignore_ascii_case(login)),
            _ => false,
        }
    }

    fn visible_announcement_jobs(&self, client: &Client) -> Vec<Arc<Job>> {
        self.announcements
            .as_ref()
            .map_or_else(Vec::new, |announcements| {
                announcements
                    .jobs
                    .visible(client, self.announcement_owner(client))
            })
    }

    fn announcement_job(&self, client: &Client, id: &str) -> Result<Arc<Job>, Failure> {
        validate_id(id)?;
        self.visible_announcement_jobs(client).into_iter().find(|job| job.id == id).ok_or_else(|| {
            Failure::new(ErrorCode::NoMatch, "no retained announcement with that id is visible to this caller")
                .with_hint("Read list_announcements. Jobs are process-local and older completed receipts expire.")
        })
    }

    pub fn list_announcements(&self, client: &Client) -> Result<Vec<AnnouncementJobDto>, Failure> {
        self.guard(client).authorize("list_announcements", true)?;
        Ok(self
            .visible_announcement_jobs(client)
            .iter()
            .map(|job| job.view())
            .collect())
    }

    pub fn get_announcement(
        &self,
        client: &Client,
        id: &str,
    ) -> Result<AnnouncementJobDto, Failure> {
        self.guard(client).authorize("get_announcement", true)?;
        self.announcement_job(client, id).map(|job| job.view())
    }

    /// Idempotent intent: a repeated request cannot replay Stop or restoration.
    /// The receipt stays nonterminal until the worker has finished cleanup.
    pub fn cancel_announcement(
        &self,
        client: &Client,
        id: &str,
    ) -> Result<AnnouncementJobDto, Failure> {
        self.guard(client).authorize("cancel_announcement", false)?;
        let job = self.announcement_job(client, id)?;
        let (changed, result) = job.cancel();
        if changed {
            self.record(
                client,
                format!("cancel_announcement: {id}"),
                "allow".into(),
                result.done.clone(),
                None,
            );
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests;
