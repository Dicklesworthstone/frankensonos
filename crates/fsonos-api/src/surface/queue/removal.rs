//! Remove an explicitly identified range without rebuilding the owner's queue.
//! The speaker's UpdateID protects the write. A lost reply is reconciled by
//! a complete, coherent read; the non-idempotent removal is never replayed.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::policy::Client;
use fsonos_core::{CoreError, control};
use fsonos_proto::content::{QUEUE, browse_snapshot};
use fsonos_proto::{ProtoError, Transport, control as soap};
use serde::{Deserialize, Serialize};
use std::net::IpAddr;

use super::super::Surface;
use super::MAX_PAGE;
use crate::failure::{ErrorCode, Failure};

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

/// A confirmed removal. Queue content deletion is not currently undoable.
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

#[derive(Debug, PartialEq, Eq)]
struct Queue {
    revision: u32,
    uris: Vec<String>,
}

fn read<T: Transport + ?Sized>(transport: &T, host: IpAddr) -> Result<Queue, ProtoError> {
    let snapshot = browse_snapshot(transport, host, QUEUE)?;
    let uris = snapshot
        .objects
        .into_iter()
        .map(|item| {
            item.res
                .map(|res| res.uri)
                .filter(|uri| !uri.trim().is_empty() && uri != "NOT_IMPLEMENTED")
                .ok_or_else(|| {
                    ProtoError::Malformed(
                        "queue item has no identity for removal verification".into(),
                    )
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Queue {
        revision: snapshot.update_id,
        uris,
    })
}

fn removed(before: &Queue, after: &Queue, start: usize, end: usize) -> bool {
    start < end
        && end <= before.uris.len()
        && before.revision != after.revision
        && before.uris.len() - (end - start) == after.uris.len()
        && before.uris[..start] == after.uris[..start]
        && before.uris[end..] == after.uris[start..]
}

fn uncertain(detail: impl std::fmt::Display) -> Failure {
    Failure::new(
        ErrorCode::UpnpFault,
        format!("queue removal outcome is uncertain: {detail}; the write was not replayed"),
    )
    .with_hint("Read the queue again before deciding on another removal; do not retry blindly.")
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
        let result = self.remove_selected(client, req);
        let (decision, text) = match &result {
            Ok(done) => ("allow".to_string(), done.done.clone()),
            Err(failure) => {
                self.notice(failure);
                ("error".to_string(), failure.detail.clone())
            }
        };
        // A zone snapshot does not capture queue contents. Do not claim this
        // edit can be undone by restoring only a source and queue position.
        self.record(
            client,
            format!(
                "{REMOVE}: {} position {} count {}",
                req.zone,
                req.position,
                req.expected_uris.len(),
            ),
            decision,
            text,
            None,
        );
        result
    }

    fn remove_selected(
        &self,
        client: &Client,
        req: &QueueRemoveRequest,
    ) -> Result<QueueRemovedDto, Failure> {
        let (start, end) = req.range()?;
        let (households, group) = self.queue_households(client, &req.zone)?;
        let player = control::locate(&households, &group.coordinator)?;
        let host = player.ip;
        let zone = player.room_name.clone();
        let before = read(&*self.transport, host).map_err(CoreError::from)?;
        if before.uris.get(start..end) != Some(req.expected_uris.as_slice()) {
            return Err(Failure::invalid(
                "the queue selection is stale or out of range; nothing was removed",
            )
            .with_hint("Read the queue again and use the current positions and exact renderer URIs."));
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
                return Err(Failure::invalid(
                    "the queue revision changed since the selection; nothing was removed",
                )
                .with_hint("Read the current queue and select its current identities and revision."));
            }
        }
        self.queue_group_unchanged(host, &group)?;
        // A fresh topology read cannot pin the queue. The native UpdateID
        // check closes that race at the player instead of clearing/rebuilding.
        let sent = soap::remove_track_range_from_queue(
            &*self.transport,
            host,
            before.revision,
            req.position,
            count,
        );
        let after = read(&*self.transport, host).map_err(uncertain)?;
        if !removed(&before, &after, start, end) {
            if before == after
                && let Err(error @ ProtoError::SoapFault { .. }) = sent
            {
                return Err(Failure::from(CoreError::from(error))
                    .with_hint("Read the current queue before selecting a range to remove."));
            }
            return Err(uncertain(
                "read-back did not confirm exactly the selected deletion",
            ));
        }
        let recovered = sent.is_err();
        let remaining = u32::try_from(after.uris.len())
            .map_err(|_| uncertain("the resulting queue length does not fit u32"))?;
        let recovery = if recovered {
            "; confirmed by read-back, not replayed"
        } else {
            ""
        };
        Ok(QueueRemovedDto {
            done: format!(
                "removed {count} track(s) at position {} from {zone}'s queue; \
                 {remaining} remain (not undoable{recovery})",
                req.position,
            ),
            zone,
            position: req.position,
            count,
            remaining,
            recovered,
            undoable: false,
        })
    }
}

#[cfg(test)]
mod tests;
