//! Move an identified block with one native revision-checked write. Queue
//! content, source, play mode and position are never rebuilt by the controller.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::actions;
use fsonos_core::policy::Client;
use fsonos_core::{CoreError, control};
use fsonos_proto::content::{BrowsePage, QUEUE, browse_snapshot};
use fsonos_proto::queue::{STALE_QUEUE_FAULT, reorder_tracks};
use fsonos_proto::{ProtoError, Transport};
use serde::{Deserialize, Serialize};
use std::net::IpAddr;

use super::super::Surface;
use super::{MAX_PAGE, snapshot};
use crate::failure::{ErrorCode, Failure};

/// The policy action and MCP tool name for `POST /queue/move`.
pub const MOVE: &str = "move_in_queue";

/// Source and destination positions both refer to the caller's ORIGINAL
/// queue. Requiring its revision binds the destination as well as the source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QueueMoveRequest {
    pub zone: String,
    /// First selected entry, starting at one.
    pub position: u32,
    /// Move before this original position; original total + 1 appends.
    pub insert_before: u32,
    /// One to 100 consecutive renderer URIs from get_queue, in order.
    pub expected_uris: Vec<String>,
    /// Required opaque revision from the same get_queue observation.
    pub expected_revision: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct QueueMovedDto {
    pub done: String,
    pub zone: String,
    /// First position of the moved block AFTER the operation.
    pub position: u32,
    pub count: u32,
    pub total: u32,
    /// Revision observed after the operation; usable for the next edit.
    pub revision: String,
    pub changed: bool,
    /// A failed reply was recovered by observing the complete expected order.
    pub recovered: bool,
    pub undoable: bool,
}

impl QueueMovedDto {
    #[must_use]
    pub fn text(&self) -> String {
        format!("{}\n", self.done)
    }
}

struct Plan {
    uris: Vec<String>,
    position: u32,
    changes_order: bool,
}

impl QueueMoveRequest {
    fn validate(&self) -> Result<(), Failure> {
        let count = self.expected_uris.len();
        if self.zone.trim().is_empty()
            || self.expected_revision.trim().is_empty()
            || self.position == 0
            || self.insert_before == 0
            || count == 0
            || count > usize::try_from(MAX_PAGE).unwrap_or(100)
            || self
                .expected_uris
                .iter()
                .any(|uri| uri.trim().is_empty() || uri == "NOT_IMPLEMENTED")
        {
            return Err(Failure::invalid(
                "give a room, one-based positions, a queue revision and one to 100 renderer URIs",
            ));
        }
        Ok(())
    }

    fn plan(&self, queue: &[String]) -> Result<Plan, Failure> {
        self.validate()?;
        let start = usize::try_from(self.position - 1)
            .map_err(|_| Failure::invalid("queue position is too large"))?;
        let end = start
            .checked_add(self.expected_uris.len())
            .ok_or_else(|| Failure::invalid("queue range overflows"))?;
        let destination = usize::try_from(self.insert_before - 1)
            .map_err(|_| Failure::invalid("queue destination is too large"))?;
        if queue.get(start..end) != Some(self.expected_uris.as_slice()) {
            return Err(stale());
        }
        if destination > queue.len() {
            return Err(Failure::invalid(
                "insert_before must be at most the original queue length plus one",
            ));
        }
        let mut uris = queue.to_vec();
        // Moving before any slot of the block (or just after it) does nothing.
        let changes_order = !(start..=end).contains(&destination);
        let at = if changes_order {
            let moved: Vec<_> = uris.drain(start..end).collect();
            let at = if destination < start {
                destination
            } else {
                destination - moved.len()
            };
            uris.splice(at..at, moved);
            at
        } else {
            start
        };
        let position = u32::try_from(at + 1)
            .map_err(|_| Failure::invalid("resulting position is too large"))?;
        Ok(Plan {
            uris,
            position,
            changes_order,
        })
    }
}

struct Observed {
    page: BrowsePage,
    uris: Vec<String>,
}

fn read<T: Transport + ?Sized>(transport: &T, host: IpAddr) -> Result<Observed, ProtoError> {
    let full = browse_snapshot(transport, host, QUEUE)?;
    let total = u32::try_from(full.objects.len())
        .map_err(|_| ProtoError::Malformed("queue length exceeds u32".into()))?;
    let uris = full
        .objects
        .iter()
        .map(|item| {
            item.res
                .as_ref()
                .map(|res| res.uri.clone())
                .filter(|uri| !uri.trim().is_empty() && uri != "NOT_IMPLEMENTED")
                .ok_or_else(|| ProtoError::Malformed("queue item has no renderer identity".into()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Observed {
        page: BrowsePage {
            objects: full.objects,
            number_returned: total,
            total_matches: total,
            update_id: full.update_id,
        },
        uris,
    })
}

fn stale() -> Failure {
    Failure::new(
        ErrorCode::QueueChanged,
        "the queue selection or revision changed; no move was sent",
    )
    .with_hint("Read get_queue again and select source and destination from that revision.")
}

/// How an uncertain move's failure begins ([`uncertain`], [`wrote`]).
const UNCERTAIN: &str = "queue move outcome is uncertain";

fn uncertain(detail: impl std::fmt::Display) -> Failure {
    Failure::new(
        ErrorCode::UpnpFault,
        format!("{UNCERTAIN}: {detail}; the write was not replayed"),
    )
    .with_hint("Read the queue before choosing another move; never blindly retry this request.")
}

/// Whether a move changed the queue, or may have: a kept order and a
/// refusal wrote nothing, but an uncertain outcome may have moved tracks.
fn wrote(result: &Result<QueueMovedDto, Failure>) -> bool {
    match result {
        Ok(done) => done.changed,
        Err(failure) => failure.detail.starts_with(UNCERTAIN),
    }
}

impl Surface {
    /// Rearrange a selected block without changing any playback settings.
    /// Selected queues within the backup bounds retain durable undo. Other
    /// changed or uncertain outcomes stop undo from reaching past the edit.
    pub fn queue_move(
        &self,
        client: &Client,
        req: &QueueMoveRequest,
    ) -> Result<QueueMovedDto, Failure> {
        self.authorize_logged(client, MOVE)?;
        let _changing = self.log.as_ref().map(|log| {
            log.restoring
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        let mut kept = None;
        let mut result = self.move_selected(client, req, &mut kept);
        let (decision, text) = match &result {
            Ok(done) => ("allow".into(), done.done.clone()),
            Err(failure) => {
                self.notice(failure);
                ("allow".into(), format!("failed: {}", failure.detail))
            }
        };
        if wrote(&result) && kept.is_none() {
            kept = Some(actions::before_state_not_undoable("queue edit"));
        }
        let saved = self.record_confirmed(
            client,
            format!(
                "{MOVE}: {} position {} before {}",
                req.zone, req.position, req.insert_before
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

    fn move_selected(
        &self,
        client: &Client,
        req: &QueueMoveRequest,
        kept: &mut Option<String>,
    ) -> Result<QueueMovedDto, Failure> {
        req.validate()?;
        let (households, group) = self.queue_households(client, &req.zone)?;
        let player = control::locate(&households, &group.coordinator)?;
        let before = read(&*self.transport, player.ip).map_err(CoreError::from)?;
        if snapshot::revision(&group.coordinator, &before.page) != req.expected_revision {
            return Err(stale());
        }
        let plan = req.plan(&before.uris)?;
        let count = u32::try_from(req.expected_uris.len())
            .map_err(|_| Failure::invalid("too many queue items"))?;
        let mut after = before;
        let mut recovered = false;
        if plan.changes_order {
            if after.page.update_id == 0 {
                return Err(Failure::invalid(
                    "a queue move requires a nonzero native revision",
                ));
            }
            let backup = self.log.as_ref().and_then(|_| {
                actions::capture_queue_undo(
                    &*self.transport,
                    &households,
                    &group.coordinator,
                    self.now(),
                )
                .ok()
                .filter(|pending| pending.matches_before(after.page.update_id, &after.uris))
                .and_then(|pending| {
                    pending
                        .movement(req.position, count, req.insert_before)
                        .ok()
                })
            });
            self.queue_group_unchanged(player.ip, &group)?;
            let sent = reorder_tracks(
                &*self.transport,
                player.ip,
                req.position,
                count,
                req.insert_before,
                after.page.update_id,
            );
            // The player refuses an old revision (fault 1028): nothing moved.
            if matches!(&sent, Err(ProtoError::SoapFault { code, .. }) if *code == STALE_QUEUE_FAULT)
            {
                return Err(Failure::new(
                    ErrorCode::QueueChanged,
                    "the queue changed before the move reached the player, which refused the old revision; nothing was moved",
                )
                .with_hint("Read get_queue again and select source and destination from that revision."));
            }
            let observed = read(&*self.transport, player.ip).map_err(uncertain)?;
            self.queue_group_unchanged(player.ip, &group)
                .map_err(uncertain)?;
            if observed.uris != plan.uris || observed.page.update_id == after.page.update_id {
                if observed.uris == after.uris
                    && observed.page.update_id == after.page.update_id
                    && let Err(error @ ProtoError::SoapFault { .. }) = sent
                {
                    return Err(Failure::from(CoreError::from(error))
                        .with_hint("Read the queue again before selecting a new move."));
                }
                return Err(uncertain(
                    "read-back did not confirm the complete expected order",
                ));
            }
            recovered = sent.is_err();
            // Indistinguishable duplicate entries cannot prove which copy was
            // moved after a lost reply. Acknowledged native success can.
            if recovered && after.uris == plan.uris {
                return Err(uncertain(
                    "identical queue entries make the lost reply ambiguous",
                ));
            }
            after = observed;
            if let Some(backup) = backup
                && let Ok(undo) = actions::finish_queue_undo(
                    &*self.transport,
                    &households,
                    backup,
                    after.page.update_id,
                    &after.uris,
                    self.now(),
                )
            {
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
        let zone = player.room_name.clone();
        let status = if plan.changes_order { "moved" } else { "kept" };
        Ok(QueueMovedDto {
            done: format!(
                "{status} {count} track(s) at position {} in {zone}'s queue ({undo_status})",
                plan.position,
            ),
            zone,
            position: plan.position,
            count,
            total: after.page.total_matches,
            revision: snapshot::revision(&group.coordinator, &after.page),
            changed: plan.changes_order,
            recovered,
            undoable,
        })
    }
}

#[cfg(test)]
mod tests;
