//! Persist exact enqueue ownership, and rebase it only through proved undo.

use super::{Snapshot, identity, range, read, remove};
use crate::CoreError;
use crate::control::SourceDestination;
use crate::scenes::queue_backup::{MAX_QUEUE_BACKUP_BYTES, MAX_QUEUE_BACKUP_ENTRIES};
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::PlayerId;
use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use std::net::IpAddr;

/// The exact insertion result retained by an action log. A matching song at
/// the same position is insufficient after another controller replaces a queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedProof {
    coordinator: PlayerId,
    host: IpAddr,
    destination: SourceDestination,
    before: Snapshot,
    after: Snapshot,
    /// Original post-add revision; rebasing changes only the current after image.
    inserted_revision: u32,
    insertions: u32,
    first: u32,
    count: u32,
}

fn bounded(queue: &Snapshot) -> bool {
    queue.uris.len() <= MAX_QUEUE_BACKUP_ENTRIES
        && queue.uris.iter().all(|uri| identity(uri))
        && queue.uris.iter().try_fold(0usize, |bytes, uri| {
            bytes.checked_add(uri.len())
        }).is_some_and(|bytes| bytes <= MAX_QUEUE_BACKUP_BYTES)
}

/// Bound the serialized receipt without allocating another copy of its data.
fn fits(value: &impl Serialize) -> bool {
    struct Size(usize);
    impl Write for Size {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 = self.0.checked_add(bytes.len())
                .filter(|size| *size <= MAX_QUEUE_BACKUP_BYTES)
                .ok_or_else(|| io::Error::other("queued-track undo receipt exceeds its byte bound"))?;
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(&mut Size(0), value).is_ok()
}

fn unavailable(detail: &str) -> CoreError {
    CoreError::NotUndoable(format!(
        "queued-track undo unavailable: {detail}; force cannot bypass queue ownership"
    ))
}

impl QueuedProof {
    /// Only an enqueue path that proved its insertion may issue this receipt.
    /// Every accepted AddURIToQueue must advance the native revision once;
    /// a gap cannot prove that an identical external replacement did not occur.
    /// Unknown boot/address identity, wildcard revisions or oversized snapshots
    /// leave the successful add intact but unavailable for a later undo.
    #[allow(clippy::too_many_arguments)] // the native insertion and its complete observations
    pub(crate) fn confirmed(
        host: IpAddr,
        coordinator: &PlayerId,
        destination: &SourceDestination,
        before: &Snapshot,
        after: &Snapshot,
        first: u32,
        count: u32,
        insertions: u32,
    ) -> Option<Self> {
        if !bounded(before) || !bounded(after)
            || !destination.has_boot_identity() || !destination.is_at(host)
            || before.revision.checked_add(insertions) != Some(after.revision)
        {
            return None;
        }
        let proof = Self {
            coordinator: coordinator.clone(),
            host,
            destination: destination.clone(),
            before: before.clone(),
            after: after.clone(),
            inserted_revision: after.revision,
            insertions,
            first,
            count,
        };
        (proof.valid() && fits(&proof)).then_some(proof)
    }

    fn valid(&self) -> bool {
        let Ok(selected) = range(self.first, self.count) else {
            return false;
        };
        bounded(&self.before)
            && bounded(&self.after)
            && self.destination.has_boot_identity()
            && self.destination.is_at(self.host)
            && self.after.revision >= self.inserted_revision
            && self.inserted_revision != 0
            && self.insertions > 0
            && usize::try_from(self.insertions).is_ok_and(|count| {
                count <= crate::control::MAX_SPOTIFY_TRACK_BATCH
            })
            && self.before.revision.checked_add(self.insertions) == Some(self.inserted_revision)
            && self.before.revision != self.after.revision
            && selected.start <= self.before.uris.len()
            && selected.end <= self.after.uris.len()
            && self.before.uris.len().checked_add(selected.len()) == Some(self.after.uris.len())
            && self.before.uris[..selected.start] == self.after.uris[..selected.start]
            && self.before.uris[selected.start..] == self.after.uris[selected.end..]
    }

    pub(crate) fn confirm<T: Transport + ?Sized>(
        &self,
        t: &T,
        host: IpAddr,
        coordinator: &PlayerId,
        first: u32,
        uris: &[String],
    ) -> Result<(), CoreError> {
        let selected = range(self.first, self.count)
            .map_err(|_| unavailable("the saved insertion range is invalid"))?;
        if !self.valid()
            || !fits(self)
            || self.coordinator != *coordinator
            || self.host != host
            || self.first != first
            || usize::try_from(self.count).ok() != Some(uris.len())
            || self.after.uris.get(selected) != Some(uris)
        {
            return Err(unavailable("the saved insertion identity is incomplete"));
        }
        self.destination
            .confirm(t, host, coordinator)
            .map_err(|_| unavailable("the original group, source or speaker boot changed"))?;
        let current = read(t, host)
            .map_err(|_| unavailable("the current queue could not be verified"))?;
        if current != self.after {
            return Err(unavailable("the queue changed after the recorded insertion"));
        }
        self.destination
            .confirm(t, host, coordinator)
            .map_err(|_| unavailable("the original group, source or speaker boot changed"))
    }

    pub(crate) fn undo<T: Transport + ?Sized>(
        &self,
        t: &T,
        host: IpAddr,
        coordinator: &PlayerId,
        first: u32,
        uris: &[String],
    ) -> Result<Option<QueuedRestoration>, CoreError> {
        self.confirm(t, host, coordinator, first, uris)?;
        let guarded = QueuedUndoTransport { t, proof: self };
        let removed = remove(&guarded, host, coordinator, &self.after, first, self.count)?;
        self.destination.confirm(t, host, coordinator).map_err(|error| {
            CoreError::Uncertain(format!(
                "queued tracks were removed but final group ownership could not be confirmed: {error}"
            ))
        })?;
        // A second controller may have replaced identical contents after the
        // native remove. Such a revision gap cannot authorize an older undo.
        if self.after.revision.checked_add(1) != Some(removed.revision) {
            return Ok(None);
        }
        let transition = QueuedRestoration {
            coordinator: self.coordinator.clone(),
            host: self.host,
            destination: self.destination.clone(),
            previous: self.before.clone(),
            restored: Snapshot {
                revision: removed.revision,
                uris: self.before.uris.clone(),
            },
        };
        Ok((transition.valid() && fits(&transition)).then_some(transition))
    }

    /// An undo that restored this exact earlier snapshot can advance its
    /// native counter. Matching contents without that logged transition cannot.
    pub(crate) fn rebase(&mut self, transition: &QueuedRestoration) {
        if self.valid()
            && transition.valid()
            && fits(transition)
            && self.coordinator == transition.coordinator
            && self.host == transition.host
            && self.destination == transition.destination
            && self.after == transition.previous
        {
            self.after = transition.restored.clone();
        }
    }
}

/// A private action-log receipt issued only after one fully verified removal.
/// It lets successive undos recognize their own exact restored queue revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedRestoration {
    coordinator: PlayerId,
    host: IpAddr,
    destination: SourceDestination,
    previous: Snapshot,
    restored: Snapshot,
}

impl QueuedRestoration {
    fn valid(&self) -> bool {
        self.destination.has_boot_identity()
            && self.destination.is_at(self.host)
            && bounded(&self.previous)
            && bounded(&self.restored)
            && self.restored.revision != 0
            && self.previous.revision < self.restored.revision
            && self.previous.uris == self.restored.uris
    }
}

/// Check boot/group/source immediately before the one native deletion. The
/// speaker's nonzero UpdateID guards queue edits after the complete read.
struct QueuedUndoTransport<'a, T: ?Sized> {
    t: &'a T,
    proof: &'a QueuedProof,
}

impl<T: Transport + ?Sized> Transport for QueuedUndoTransport<'_, T> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if action.trim_matches('"').ends_with("#RemoveTrackRangeFromQueue") {
            if host != self.proof.host {
                return Err(ProtoError::Malformed(
                    "the queued-track undo target moved".into(),
                ));
            }
            self.proof.destination.confirm(self.t, host, &self.proof.coordinator)?;
        }
        self.t.soap_post(host, path, action, body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn destination(boot: Option<u32>, host: Option<IpAddr>) -> SourceDestination {
        serde_json::from_value(serde_json::json!({
            "members": ["RINCON_SYNTHETIC"],
            "boot_seq": boot,
            "coordinator_host": host,
            "uri": "",
            "metadata": ""
        })).unwrap()
    }

    #[test]
    fn receipts_require_known_boot_address_nonzero_revision_and_bounded_contents() {
        let host: IpAddr = "127.0.0.1".parse().unwrap();
        let coordinator = PlayerId("RINCON_SYNTHETIC".into());
        let before = Snapshot { revision: 0, uris: Vec::new() };
        let after = Snapshot { revision: 1, uris: vec!["test:track".into()] };
        let make = |destination: &SourceDestination, after: &Snapshot| {
            QueuedProof::confirmed(host, &coordinator, destination, &before, after, 1, 1, 1)
        };
        assert!(make(&destination(Some(1), Some(host)), &after).is_some());
        // Identical-looking contents at a later counter are not our insertion.
        assert!(make(&destination(Some(1), Some(host)), &Snapshot {
            revision: 3, ..after.clone()
        }).is_none());
        assert!(make(&destination(None, Some(host)), &after).is_none());
        assert!(make(&destination(Some(1), None), &after).is_none());
        assert!(make(&destination(Some(1), Some("127.0.0.2".parse().unwrap())), &after).is_none());
        assert!(make(&destination(Some(1), Some(host)), &Snapshot {
            revision: 0, ..after.clone()
        }).is_none());
        let oversized = Snapshot {
            revision: 2, uris: vec!["test:track".into(); MAX_QUEUE_BACKUP_ENTRIES + 1]
        };
        assert!(make(&destination(Some(1), Some(host)), &oversized).is_none());
        let wrapped_before = Snapshot { revision: u32::MAX, uris: Vec::new() };
        let wrapped_after = Snapshot { revision: 1, uris: vec!["test:a".into(), "test:b".into()] };
        assert!(QueuedProof::confirmed(
            host, &coordinator, &destination(Some(1), Some(host)),
            &wrapped_before, &wrapped_after, 1, 2, 2,
        ).is_none());
        let mut huge = destination(Some(1), Some(host));
        // Serialized metadata is bounded too, including JSON escaping.
        let mut value = serde_json::to_value(&huge).unwrap();
        value["metadata"] = serde_json::json!("\"".repeat(MAX_QUEUE_BACKUP_BYTES / 2));
        huge = serde_json::from_value(value).unwrap();
        assert!(make(&huge, &after).is_none());
    }
}
