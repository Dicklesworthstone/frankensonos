//! Remove an explicitly identified range without rebuilding the owner's queue.
//! The speaker's UpdateID protects the write. A lost reply is reconciled by
//! a complete, coherent read; the non-idempotent removal is never replayed.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::actions;
use fsonos_core::control::queue_removal::{self, RemovalError, read};
use fsonos_core::policy::Client;
use fsonos_core::{CoreError, control};
use serde::{Deserialize, Serialize};

use super::super::Surface;
use super::MAX_PAGE;
use crate::failure::{ErrorCode, Failure};
use fsonos_proto::queue::STALE_QUEUE_FAULT;

#[cfg(test)]
use fsonos_core::control::queue_removal::{Snapshot as Queue, removed};
#[cfg(test)]
use fsonos_proto::{ProtoError, Transport, control as soap};
#[cfg(test)]
use std::net::IpAddr;

/// The policy tool name for `POST /queue/remove`.
pub const REMOVE: &str = "remove_from_queue";

/// Remove exactly these consecutive items, as observed by `get_queue`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QueueRemoveRequest {
    pub zone: String,
    /// The first queue position to remove, starting at one.
    pub position: u32,
    /// Exact renderer URIs from `get_queue`, in order (one to 100 items).
    /// These are mandatory: a shifted position must not remove other music.
    pub expected_uris: Vec<String>,
    /// Opaque `revision` from `get_queue`. When supplied, the coordinator,
    /// revision and queue length must still match that observation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<String>,
}

impl QueueRemoveRequest {
    fn range(&self) -> Result<(usize, usize), Failure> {
        if self.zone.trim().is_empty() || self.position == 0 {
            return Err(Failure::invalid(
                "give a room and a queue position starting at one",
            ));
        }
        if self
            .expected_revision
            .as_deref()
            .is_some_and(|revision| revision.trim().is_empty())
        {
            return Err(Failure::invalid("expected_revision must not be blank"));
        }
        let count = u32::try_from(self.expected_uris.len())
            .map_err(|_| Failure::invalid("too many queue items to remove"))?;
        if !(1..=MAX_PAGE).contains(&count)
            || self
                .expected_uris
                .iter()
                .any(|uri| uri.trim().is_empty() || uri == "NOT_IMPLEMENTED")
        {
            return Err(Failure::invalid(
                "expected_uris must contain one to 100 nonempty renderer URIs from get_queue",
            ));
        }
        let start = self.position - 1;
        let end = start
            .checked_add(count)
            .ok_or_else(|| Failure::invalid("queue range overflows"))?;
        Ok((
            usize::try_from(start).map_err(|_| Failure::invalid("queue position is too large"))?,
            usize::try_from(end).map_err(|_| Failure::invalid("queue position is too large"))?,
        ))
    }
}

/// A confirmed removal. `undoable` requires a durable complete queue backup
/// and proof of the exact result, still subject to ownership checks on undo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct QueueRemovedDto {
    pub done: String,
    pub zone: String,
    pub position: u32,
    pub count: u32,
    /// Queue length observed after removal, not a prediction.
    pub remaining: u32,
    /// The write reply failed, but read-back confirmed the requested result.
    pub recovered: bool,
    pub undoable: bool,
}

impl QueueRemovedDto {
    #[must_use]
    pub fn text(&self) -> String {
        format!("{}\n", self.done)
    }
}

/// How an uncertain removal's failure begins ([`uncertain`]).
const UNCERTAIN: &str = "queue removal outcome is uncertain";

fn uncertain(detail: impl std::fmt::Display) -> Failure {
    Failure::new(
        ErrorCode::OutcomeUncertain,
        format!("{UNCERTAIN}: {detail}; the write was not replayed"),
    )
    .with_hint("Read the queue again before deciding on another removal; do not retry blindly.")
}

fn removal_failure(error: RemovalError) -> Failure {
    match error {
        RemovalError::Invalid(detail) => Failure::invalid(detail),
        // The player refuses an old revision (fault 1028): nothing removed.
        RemovalError::Refused(fsonos_proto::ProtoError::SoapFault {
            code: STALE_QUEUE_FAULT,
            ..
        }) => Failure::new(
            ErrorCode::QueueChanged,
            "the queue changed before the removal reached the player, which refused the old revision; nothing was removed",
        )
        .with_hint("Read the current queue before selecting a range to remove."),
        RemovalError::Before(cause) | RemovalError::Refused(cause) => {
            Failure::from(CoreError::from(cause))
                .with_hint("Read the current queue before selecting a range to remove.")
        }
        RemovalError::Uncertain(detail) => uncertain(detail),
    }
}

impl Surface {
    /// Remove a range once, addressing fresh topology's coordinator, with
    /// the queue revision the selected identities belong to. No clear,
    /// re-enqueue, source, seek, volume or transport write is issued here.
    pub fn queue_remove(
        &self,
        client: &Client,
        req: &QueueRemoveRequest,
    ) -> Result<QueueRemovedDto, Failure> {
        self.authorize_logged(client, REMOVE)?;
        let _changing = self.log.as_ref().map(|log| {
            log.restoring
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        let mut kept = None;
        let mut result = self.remove_selected(client, req, &mut kept);
        let (decision, text) = match &result {
            Ok(done) => ("allow".to_string(), done.done.clone()),
            Err(failure) => {
                self.notice(failure);
                ("allow".to_string(), format!("failed: {}", failure.detail))
            }
        };
        // Unsupported captures and uncertain writes retain the barrier. A
        // reference-only snapshot must never claim to recover deleted entries.
        let wrote = match &result {
            Ok(_) => true,
            Err(failure) => failure.detail.starts_with(UNCERTAIN),
        };
        if wrote && kept.is_none() {
            kept = Some(actions::before_state_not_undoable("queue edit"));
        }
        let saved = self.record_confirmed(
            client,
            format!(
                "{REMOVE}: {} position {} count {}",
                req.zone,
                req.position,
                req.expected_uris.len(),
            ),
            decision,
            text,
            kept,
        );
        if !saved
            && let Ok(done) = &mut result
            && done.undoable
        {
            done.undoable = false;
            done.done = done.done.replace(
                "undo available",
                "not undoable: the action log could not be saved",
            );
        }
        result
    }

    #[allow(clippy::too_many_lines)] // the queue-undo backup's staging is in flight
    fn remove_selected(
        &self,
        client: &Client,
        req: &QueueRemoveRequest,
        kept: &mut Option<String>,
    ) -> Result<QueueRemovedDto, Failure> {
        let (start, end) = req.range()?;
        let (households, group) = self.queue_households(client, &req.zone)?;
        let player = control::locate(&households, &group.coordinator)?;
        let host = player.ip;
        let zone = player.room_name.clone();
        let before = read(&*self.transport, host).map_err(CoreError::from)?;
        if before.uris.get(start..end) != Some(req.expected_uris.as_slice()) {
            return Err(Failure::new(
                ErrorCode::QueueChanged,
                "the queue selection is stale or out of range; nothing was removed",
            )
            .with_hint(
                "Read the queue again and use the current positions and exact renderer URIs.",
            ));
        }
        // UpdateID=0 is the protocol's wildcard, not a compare-and-swap.
        // Never turn an observed zero (including counter wrap) into an
        // unguarded removal that could delete a concurrently shifted range.
        if before.revision == 0 {
            return Err(Failure::invalid(
                "the player reports wildcard queue revision zero; nothing was removed",
            )
            .with_hint("A revision-checked removal requires a nonzero queue revision."));
        }
        let count = u32::try_from(end - start)
            .map_err(|_| Failure::invalid("too many queue items to remove"))?;
        if let Some(expected) = &req.expected_revision {
            // Reuse the queue reader's token contract rather than parsing or
            // independently reconstructing its opaque coordinator identity.
            let page = super::snapshot::page(&*self.transport, host, req.position - 1, count)?;
            if page.update_id != before.revision
                || usize::try_from(page.total_matches).ok() != Some(before.uris.len())
                || super::snapshot::revision(&group.coordinator, &page) != *expected
            {
                return Err(Failure::new(
                    ErrorCode::QueueChanged,
                    "the queue revision changed since the selection; nothing was removed",
                )
                .with_hint(
                    "Read the current queue and select its current identities and revision.",
                ));
            }
        }
        let backup = self.log.as_ref().and_then(|_| {
            actions::capture_queue_undo(
                &*self.transport,
                &households,
                &group.coordinator,
                self.now(),
            )
            .ok()
            .filter(|pending| pending.matches_before(before.revision, &before.uris))
            .and_then(|pending| pending.removal(req.position, count).ok())
        });
        self.queue_group_unchanged(host, &group)?;
        // The same native mutation and read-back as enqueue undo. The API
        // retains its stricter request, revision-token and membership checks.
        let done = queue_removal::remove(
            &*self.transport,
            host,
            &group.coordinator,
            &before,
            req.position,
            count,
        )
        .map_err(removal_failure)?;
        if let Some(backup) = backup {
            let expected: Vec<_> = before.uris[..start]
                .iter()
                .chain(&before.uris[end..])
                .cloned()
                .collect();
            if let Ok(undo) = actions::finish_queue_undo(
                &*self.transport,
                &households,
                backup,
                done.revision,
                &expected,
                self.now(),
            ) {
                *kept = actions::with_queue_undo(None, &[undo]);
            }
        }
        let undoable = kept
            .as_deref()
            .is_some_and(|state| actions::undoable(Some(state)));
        let undo_status = if undoable {
            "undo available"
        } else {
            "not undoable"
        };
        let (remaining, recovered) = (done.remaining, done.recovered);
        let recovery = if recovered {
            "; confirmed by read-back, not replayed"
        } else {
            ""
        };
        Ok(QueueRemovedDto {
            done: format!(
                "removed {count} track(s) at position {} from {zone}'s queue; \
                 {remaining} remain ({undo_status}{recovery})",
                req.position,
            ),
            zone,
            position: req.position,
            count,
            remaining,
            recovered,
            undoable,
        })
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod authority_tests;

#[cfg(test)]
mod undo_tests;
