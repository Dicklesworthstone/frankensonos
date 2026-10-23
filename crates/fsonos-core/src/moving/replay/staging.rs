//! Copy a complete queue before committing its replacement. Failed staging
//! can leave a suffix, but never authorizes clearing the original prefix.

use super::{
    Item, MoveError, Prepared, Source, invalid, mode,
    owner::{Action, Owner},
    verify,
};
use crate::CoreError;
use crate::control::queue_removal::{self, Snapshot};
use fsonos_proto::content::{self, BrowseFlag, QUEUE};
use fsonos_proto::control as soap;
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::{PlayerId, TransportState};
use std::cell::Cell;
use std::net::IpAddr;
use std::time::Instant;

fn head<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    revision: u32,
    count: usize,
) -> Result<(), ProtoError> {
    let page = content::browse(t, host, QUEUE, BrowseFlag::DirectChildren, 0, 1)?;
    if page.update_id != revision || usize::try_from(page.total_matches).ok() != Some(count) {
        return Err(ProtoError::Malformed(
            "the destination queue changed while staging the move".into(),
        ));
    }
    Ok(())
}

/// Helpers may do their own reads before writing. Fence the actual mutation,
/// not just entry to the helper, and never mistake a failed fence for a lost
/// acknowledgement from a write that was sent.
struct Fenced<'a, T: ?Sized> {
    inner: &'a T,
    owner: &'a Owner,
    host: IpAddr,
    target: &'a PlayerId,
    revision: u32,
    count: usize,
    action: &'a str,
    blocked: Cell<bool>,
    sent: Cell<bool>,
}

impl<T: Transport + ?Sized> Transport for Fenced<'_, T> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action
            .trim_matches('"')
            .rsplit('#')
            .next()
            .unwrap_or(action);
        if !name.starts_with("Get") && name != "Browse" {
            let proof = (|| {
                if host != self.host || name != self.action {
                    return Err(ProtoError::Malformed(
                        "an unexpected queue transfer mutation was requested".into(),
                    ));
                }
                self.owner.confirm(self.inner, host, self.target)?;
                head(self.inner, host, self.revision, self.count)
            })();
            if proof.is_err() {
                self.blocked.set(true);
            }
            proof?;
            self.sent.set(true);
        }
        self.inner.soap_post(host, path, action, body)
    }
}

fn complete(before: &Snapshot, items: &[Item], after: &Snapshot) -> bool {
    before.uris.len().checked_add(items.len()) == Some(after.uris.len())
        && after.uris.starts_with(&before.uris)
        && after.uris[before.uris.len()..]
            .iter()
            .zip(items)
            .all(|(uri, item)| verify::identity(uri) == verify::identity(&item.uri))
}

struct Stage<'a, T: ?Sized> {
    t: &'a T,
    host: IpAddr,
    target: &'a PlayerId,
    owner: &'a Owner,
    before: &'a Snapshot,
    items: &'a [Item],
}

impl<T: Transport + ?Sized> Stage<'_, T> {
    fn guarded(&self, revision: u32, count: usize, action: &'static str) -> Fenced<'_, T> {
        Fenced {
            inner: self.t,
            owner: self.owner,
            host: self.host,
            target: self.target,
            revision,
            count,
            action,
            blocked: Cell::new(false),
            sent: Cell::new(false),
        }
    }

    fn append(&self, index: usize, revision: u32) -> Result<u32, MoveError> {
        let prior_len = self.before.uris.len() + index;
        let guarded = self.guarded(revision, prior_len, "AddURIToQueue");
        let item = &self.items[index];
        let uncertain =
            match soap::enqueue_uri(&guarded, self.host, &item.uri, &item.metadata, 0, false) {
                Ok(added)
                    if added.count == 1
                        && usize::try_from(added.first).ok() == Some(prior_len + 1) =>
                {
                    false
                }
                Ok(_) => {
                    return Err(invalid(
                        "the destination did not append exactly one occurrence",
                    ));
                }
                Err(error) if guarded.blocked.get() => return Err(error.into()),
                Err(ProtoError::Network { .. } | ProtoError::Malformed(_)) => true,
                Err(error) => return Err(error.into()),
            };
        let observed = if uncertain {
            let after = queue_removal::read(self.t, self.host)?;
            if !complete(self.before, &self.items[..=index], &after) {
                return Err(invalid(
                    "the full destination queue did not prove the uncertain append; it was not replayed",
                ));
            }
            after.revision
        } else {
            let offset = u32::try_from(prior_len)
                .map_err(|_| invalid("the destination queue is too large"))?;
            let after = content::browse(
                self.t,
                self.host,
                QUEUE,
                BrowseFlag::DirectChildren,
                offset,
                1,
            )?;
            if after.number_returned != 1
                || after.objects.len() != 1
                || usize::try_from(after.total_matches).ok() != Some(prior_len + 1)
                || after.objects[0]
                    .res
                    .as_ref()
                    .is_none_or(|res| verify::identity(&res.uri) != verify::identity(&item.uri))
            {
                return Err(invalid("the appended queue occurrence was not confirmed"));
            }
            after.update_id
        };
        if observed == revision {
            return Err(invalid("the destination append has no new queue revision"));
        }
        self.owner.confirm(self.t, self.host, self.target)?;
        head(self.t, self.host, observed, prior_len + 1)?;
        Ok(observed)
    }

    fn commit(&self, revision: u32) -> Result<(u32, Owner), MoveError> {
        let staged = queue_removal::read(self.t, self.host)?;
        if staged.revision != revision || !complete(self.before, self.items, &staged) {
            return Err(invalid(
                "the original prefix and entire incoming queue were not both confirmed",
            ));
        }
        self.owner.confirm(self.t, self.host, self.target)?;
        if !self.before.uris.is_empty() {
            let guarded = self.guarded(revision, staged.uris.len(), "RemoveTrackRangeFromQueue");
            let count = u32::try_from(self.before.uris.len())
                .map_err(|_| invalid("the destination queue is too large"))?;
            let removed =
                queue_removal::remove(&guarded, self.host, self.target, &staged, 1, count);
            if guarded.blocked.get() {
                return Err(invalid(
                    "the destination ownership fence blocked removal; another controller's deletion is not this transfer",
                ));
            }
            removed.map_err(CoreError::from)?;
        }
        let final_revision = verify::queue(self.t, self.host, self.items)?;
        let committed = self.owner.after_removal(
            self.t,
            self.host,
            self.target,
            self.before.uris.len(),
            self.items,
        )?;
        Ok((final_revision, committed))
    }
}

pub(super) fn replace<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    target: &PlayerId,
    items: &[Item],
    mode_known: bool,
) -> Result<(u32, Owner), MoveError> {
    let owner = Owner::read(t, host, target, mode_known)?;
    let before = queue_removal::read(t, host)?;
    let limit = usize::try_from(content::MAX_BROWSE_OBJECTS).unwrap_or(usize::MAX);
    if items.is_empty()
        || before
            .uris
            .len()
            .checked_add(items.len())
            .is_none_or(|len| len > limit)
    {
        return Err(invalid(
            "the destination has no room for a complete, verifiable staged queue",
        ));
    }
    let guarded = Fenced {
        inner: t,
        owner: &owner,
        host,
        target,
        revision: before.revision,
        count: before.uris.len(),
        action: "Stop",
        blocked: Cell::new(false),
        sent: Cell::new(false),
    };
    // Stop before appending, so old playback cannot run into the staged
    // suffix. A paused source is never audibly started to establish pause.
    match soap::stop(&guarded, host) {
        Ok(()) => {}
        Err(error) if guarded.blocked.get() => return Err(error.into()),
        Err(ProtoError::Network { .. } | ProtoError::Malformed(_)) => {}
        Err(error) => return Err(error.into()),
    }
    let quiet = owner.after_stop(t, host, target)?;
    let stage = Stage {
        t,
        host,
        target,
        owner: &quiet,
        before: &before,
        items,
    };
    let mut revision = before.revision;
    for index in 0..items.len() {
        revision = stage.append(index, revision)?;
    }
    stage.commit(revision)
}

/// Carry staging's exact ownership evidence through the final source, mode,
/// selection and transport writes. A failed proof permanently poisons this run.
struct Finishing<'a, T: ?Sized> {
    t: &'a T,
    host: IpAddr,
    target: &'a PlayerId,
    revision: u32,
    count: usize,
    owner: Owner,
    poisoned: bool,
}

impl<T: Transport + ?Sized> Finishing<'_, T> {
    fn confirm(&self) -> Result<(), MoveError> {
        if self.poisoned {
            return Err(invalid(
                "queue finalization lost ownership; no further writes are allowed",
            ));
        }
        self.owner.confirm(self.t, self.host, self.target)?;
        head(self.t, self.host, self.revision, self.count)?;
        Ok(())
    }

    fn step(
        &mut self,
        action: Action<'_>,
        operation: impl FnOnce(&Fenced<'_, T>) -> Result<(), MoveError>,
    ) -> Result<(), MoveError> {
        self.confirm()?;
        self.poisoned = true;
        let started = Instant::now();
        let (written, blocked, sent) = {
            let guarded = Fenced {
                inner: self.t,
                owner: &self.owner,
                host: self.host,
                target: self.target,
                revision: self.revision,
                count: self.count,
                action: action.name(),
                blocked: Cell::new(false),
                sent: Cell::new(false),
            };
            (
                operation(&guarded),
                guarded.blocked.get(),
                guarded.sent.get(),
            )
        };
        // Mode restoration can reconcile a lost response using fresh reads.
        // It must not turn our blocked write into somebody else's success.
        if blocked {
            return Err(invalid(
                "the final playback ownership fence blocked a write",
            ));
        }
        written?;
        if matches!(action, Action::Play) {
            verify::transport(self.t, self.host, TransportState::Playing)?;
        }
        let next = self.owner.after_action(
            self.t,
            self.host,
            self.target,
            sent.then_some(action),
            started,
        )?;
        head(self.t, self.host, self.revision, self.count)?;
        self.owner = next;
        self.poisoned = false;
        Ok(())
    }
}

pub(super) fn finish<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    target: &PlayerId,
    prepared: &Prepared,
    revision: u32,
    owner: Owner,
) -> Result<(), MoveError> {
    let Source::Queue {
        items,
        track,
        position,
    } = &prepared.source
    else {
        return Err(invalid("queue finalization requires a prepared queue"));
    };
    let selected = track
        .checked_sub(1)
        .and_then(|index| usize::try_from(index).ok())
        .and_then(|index| items.get(index))
        .ok_or_else(|| invalid("the prepared queue occurrence is missing"))?;
    let first = items
        .first()
        .ok_or_else(|| invalid("the prepared queue is empty"))?;
    let mut finishing = Finishing {
        t,
        host,
        target,
        revision,
        count: items.len(),
        owner,
        poisoned: false,
    };
    finishing.step(Action::Load(first), |guarded| {
        soap::play_from_queue(guarded, host, target).map_err(MoveError::from)
    })?;
    if let Some(mode) = prepared.play_mode {
        finishing.step(Action::Mode(mode), |guarded| {
            mode::apply(guarded, host, target, revision, mode)
        })?;
    }
    finishing.step(Action::Track(*track, selected), |guarded| {
        soap::seek_track(guarded, host, *track).map_err(MoveError::from)
    })?;
    if *position > 0 {
        finishing.step(Action::Position(*position), |guarded| {
            soap::seek_position(guarded, host, *position).map_err(MoveError::from)
        })?;
    }
    let started = Instant::now();
    let state = if prepared.transport == TransportState::Playing {
        finishing.step(Action::Play, |guarded| {
            soap::play(guarded, host).map_err(MoveError::from)
        })?;
        TransportState::Playing
    } else {
        TransportState::Stopped
    };
    finishing.confirm()?;
    verify::transport(t, host, state)?;
    verify::playback(t, host, target, prepared, Some(revision), started.elapsed())
}
