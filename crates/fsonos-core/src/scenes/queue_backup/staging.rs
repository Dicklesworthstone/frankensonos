//! Reconstruction writes and the proof required before the original prefix
//! can be removed. Successful append replies use one bounded slot read;
//! uncertain replies require the whole expected queue, without another write.

use super::{Owner, QueueBackup, invalid, queue, same_item, same_items};
use crate::control::queue_removal;
use crate::{CoreError, HouseholdState, control};
use fsonos_proto::content::{self, BrowseFlag, BrowseSnapshot, QUEUE};
use fsonos_proto::control as soap;
use fsonos_proto::didl::DidlObject;
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::{PlayerId, TransportState};
use std::cell::Cell;
use std::net::IpAddr;

/// Fence the actual mutation, after helpers have performed their own reads.
/// Reads deliberately use the underlying transport to avoid recursion.
struct Fenced<'a, T: ?Sized> {
    t: &'a T,
    owner: &'a Owner,
    host: IpAddr,
    coordinator: &'a PlayerId,
    allowed: &'a [&'a str],
    blocked: Cell<bool>,
    stopped: Cell<bool>,
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
            if host != self.host || !self.allowed.contains(&name) {
                return Err(invalid(
                    "an unexpected reconstruction mutation was requested",
                ));
            }
            if let Err(error) = self.owner.confirm(self.t, host, self.coordinator) {
                self.blocked.set(true);
                return Err(error);
            }
        }
        if name == "Stop" {
            self.stopped.set(true);
        }
        self.t.soap_post(host, path, action, body)
    }
}

fn head<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    revision: u32,
    count: usize,
) -> Result<(), ProtoError> {
    let page = content::browse(t, host, QUEUE, BrowseFlag::DirectChildren, 0, 1)?;
    if page.update_id != revision || usize::try_from(page.total_matches).ok() != Some(count) {
        return Err(invalid("the queue changed during reconstruction"));
    }
    Ok(())
}

fn full_matches(before: &[DidlObject], suffix: &[DidlObject], actual: &[DidlObject]) -> bool {
    actual.len() == before.len() + suffix.len()
        && same_items(before, &actual[..before.len()])
        && same_items(suffix, &actual[before.len()..])
}

fn write_proof(error: Option<&ProtoError>, detail: impl std::fmt::Display) -> ProtoError {
    invalid(match error {
        Some(error) => format!("{detail} (uncertain write reply: {error})"),
        None => detail.to_string(),
    })
}

struct Stage<'a, T: ?Sized> {
    t: &'a T,
    host: IpAddr,
    coordinator: &'a PlayerId,
    owner: &'a Owner,
    before: &'a BrowseSnapshot,
    saved: &'a [DidlObject],
}

impl<T: Transport + ?Sized> Stage<'_, T> {
    fn append(&self, backup: &QueueBackup, index: usize, revision: u32) -> Result<u32, ProtoError> {
        let prior_len = self.before.objects.len() + index;
        head(self.t, self.host, revision, prior_len)?;
        let guarded = Fenced {
            t: self.t,
            owner: self.owner,
            host: self.host,
            coordinator: self.coordinator,
            allowed: &["AddURIToQueue"],
            blocked: Cell::new(false),
            stopped: Cell::new(false),
        };
        let entry = &backup.entries[index];
        let write_error =
            match soap::enqueue_uri(&guarded, self.host, &entry.uri, &entry.metadata, 0, false) {
                Ok(added)
                    if added.count == 1
                        && usize::try_from(added.first).ok() == Some(prior_len + 1) =>
                {
                    None
                }
                Ok(_) => {
                    return Err(invalid(
                        "the player did not acknowledge exactly one appended occurrence",
                    ));
                }
                Err(error) if guarded.blocked.get() => return Err(error),
                Err(error @ (ProtoError::Network { .. } | ProtoError::Malformed(_))) => Some(error),
                Err(error) => return Err(error),
            };
        let observed = if write_error.is_some() {
            let after = content::browse_snapshot(self.t, self.host, QUEUE)
                .map_err(|error| write_proof(write_error.as_ref(), error))?;
            if !full_matches(&self.before.objects, &self.saved[..=index], &after.objects) {
                return Err(write_proof(
                    write_error.as_ref(),
                    "the full queue does not prove exactly the attempted append",
                ));
            }
            after.update_id
        } else {
            let offset = u32::try_from(prior_len).map_err(invalid)?;
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
                || !same_item(&self.saved[index], &after.objects[0])
            {
                return Err(invalid(
                    "the appended occurrence or complete queue length was not confirmed",
                ));
            }
            after.update_id
        };
        if observed == revision {
            return Err(write_proof(
                write_error.as_ref(),
                "the attempted append has no new queue revision",
            ));
        }
        self.owner
            .confirm(self.t, self.host, self.coordinator)
            .map_err(|error| write_proof(write_error.as_ref(), error))?;
        head(self.t, self.host, observed, prior_len + 1)
            .map_err(|error| write_proof(write_error.as_ref(), error))?;
        Ok(observed)
    }

    fn commit(&self, revision: u32) -> Result<(u32, Owner), CoreError> {
        let staged = content::browse_snapshot(self.t, self.host, QUEUE)?;
        if staged.update_id != revision
            || !full_matches(&self.before.objects, self.saved, &staged.objects)
        {
            return Err(invalid(
                "the original prefix and complete saved suffix were not both confirmed",
            )
            .into());
        }
        self.owner.confirm(self.t, self.host, self.coordinator)?;
        let committed_revision = if self.before.objects.is_empty() {
            revision
        } else {
            let deletion = queue_removal::Snapshot {
                revision,
                uris: staged
                    .objects
                    .iter()
                    .map(|item| {
                        item.res
                            .as_ref()
                            .expect("all staged resources verified")
                            .uri
                            .clone()
                    })
                    .collect(),
            };
            let guarded = Fenced {
                t: self.t,
                owner: self.owner,
                host: self.host,
                coordinator: self.coordinator,
                allowed: &["RemoveTrackRangeFromQueue"],
                blocked: Cell::new(false),
                stopped: Cell::new(false),
            };
            let count = u32::try_from(self.before.objects.len()).map_err(invalid)?;
            let removed =
                queue_removal::remove(&guarded, self.host, self.coordinator, &deletion, 1, count)?;
            if guarded.blocked.get() {
                return Err(invalid(
                    "the source ownership fence failed before committing the queue",
                )
                .into());
            }
            removed.revision
        };
        let final_queue = content::browse_snapshot(self.t, self.host, QUEUE)?;
        if final_queue.update_id != committed_revision
            || !same_items(self.saved, &final_queue.objects)
        {
            return Err(invalid(
                "the committed queue no longer matches its confirmed revision and complete saved contents",
            )
            .into());
        }
        let committed = self.owner.after_removal(
            self.t,
            self.host,
            self.coordinator,
            self.before.objects.len(),
            &final_queue.objects,
        )?;
        committed.confirm(self.t, self.host, self.coordinator)?;
        Ok((final_queue.update_id, committed))
    }
}

fn intended(
    owner: &Owner,
    house: &HouseholdState,
    coordinator: &PlayerId,
) -> Result<(), ProtoError> {
    let mut members = house
        .groups
        .iter()
        .find(|group| group.coordinator == *coordinator)
        .map(|group| group.members.clone())
        .ok_or_else(|| invalid("the original player no longer leads the intended group"))?;
    members.sort_by(|a, b| a.0.cmp(&b.0));
    members.dedup();
    if members != owner.group.members {
        return Err(invalid(
            "the intended group membership changed before reconstruction",
        ));
    }
    Ok(())
}

fn fresh_reference(backup: &QueueBackup, revision: u32, owner: &Owner) -> queue::QueueReference {
    let mut reference = backup.reference.clone();
    reference.update_id = revision;
    reference.boot_seq = owner.group.boot_seq;
    reference
}

fn silence<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    host: IpAddr,
    coordinator: &PlayerId,
    owner: &Owner,
) -> Result<Owner, CoreError> {
    let guarded = Fenced {
        t,
        owner,
        host,
        coordinator,
        allowed: &["Pause", "Stop"],
        blocked: Cell::new(false),
        stopped: Cell::new(false),
    };
    // A playing original queue could otherwise run into the staged suffix.
    // Both playing and paused scenes stage silently, before any append.
    control::pause_confirmed(&guarded, households, coordinator)?;
    if guarded.blocked.get() {
        return Err(invalid("the source ownership fence failed before pausing").into());
    }
    let quiet = Owner::read(t, host, coordinator, owner.mode.is_some())?;
    let time_matches = if guarded.stopped.get() && quiet.playback.state == TransportState::Stopped {
        quiet.playback.position.position_secs == Some(0)
            || (owner.playback.position.position_secs.is_none()
                && quiet.playback.position.position_secs.is_none())
    } else {
        owner.playback.time_agrees(&quiet.playback)
    };
    if !time_matches
        || owner.group != quiet.group
        || owner.mode != quiet.mode
        || !owner.playback.same_source(&quiet.playback)
        || !matches!(
            quiet.playback.state,
            TransportState::Paused | TransportState::Stopped
        )
    {
        return Err(
            invalid("the original source did not become silently owned before staging").into(),
        );
    }
    Ok(quiet)
}

#[allow(clippy::too_many_arguments)] // optional ownership inherited from a logged edit
pub(super) fn restore<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    house: &HouseholdState,
    coordinator: &PlayerId,
    backup: &QueueBackup,
    start: bool,
    attempted: &mut bool,
    baseline: Option<&super::Baseline<'_>>,
) -> Result<bool, String> {
    let prepare = (|| -> Result<_, CoreError> {
        let host = control::locate(households, coordinator)?.ip;
        let owner = Owner::read(t, host, coordinator, backup.reference.play_mode.is_some())?;
        intended(&owner, house, coordinator)?;
        let before = content::browse_snapshot(t, host, QUEUE)?;
        if let Some(expected) = baseline {
            let uris = before
                .objects
                .iter()
                .map(|item| item.res.as_ref().map(|res| &res.uri))
                .collect::<Option<Vec<_>>>();
            if expected.owner.group != owner.group
                || expected.owner.mode != owner.mode
                || !expected.owner.playback.agrees(&owner.playback)
                || expected.revision != before.update_id
                || uris.as_ref().is_none_or(|uris| {
                    uris.len() != expected.uris.len()
                        || uris.iter().zip(expected.uris).any(|(a, b)| *a != b)
                })
            {
                return Err(
                    invalid("the logged queue result changed before undo reconstruction").into(),
                );
            }
        }
        if before.objects.iter().any(|item| {
            item.res
                .as_ref()
                .is_none_or(|res| res.uri.trim().is_empty() || res.uri == "NOT_IMPLEMENTED")
        }) {
            return Err(invalid(
                "the original queue has entries whose identity cannot be verified",
            )
            .into());
        }
        let saved = backup
            .entries
            .iter()
            .map(super::QueueBackupEntry::object)
            .collect::<Result<Vec<_>, _>>()?;
        owner.confirm(t, host, coordinator)?;
        if same_items(&saved, &before.objects) {
            return Ok((
                false,
                fresh_reference(backup, before.update_id, &owner),
                owner,
            ));
        }
        let limit = usize::try_from(content::MAX_BROWSE_OBJECTS).unwrap_or(usize::MAX);
        if before
            .objects
            .len()
            .checked_add(saved.len())
            .is_none_or(|len| len > limit)
        {
            return Err(
                invalid("the current queue leaves no room for a verifiable staged backup").into(),
            );
        }
        *attempted = true;
        let quiet = silence(t, households, host, coordinator, &owner)?;
        let stage = Stage {
            t,
            host,
            coordinator,
            owner: &quiet,
            before: &before,
            saved: &saved,
        };
        let mut revision = before.update_id;
        for index in 0..backup.entries.len() {
            revision = stage.append(backup, index, revision)?;
        }
        let (final_revision, committed) = stage.commit(revision)?;
        Ok((
            true,
            fresh_reference(backup, final_revision, &committed),
            committed,
        ))
    })()
    .map_err(|error| error.to_string())?;
    let (rebuilt, reference, owner) = prepare;
    queue::restore_from(
        t,
        households,
        coordinator,
        &reference,
        start,
        Some((&owner.playback, owner.mode)),
    )
    .map(|played| rebuilt || played)
}
