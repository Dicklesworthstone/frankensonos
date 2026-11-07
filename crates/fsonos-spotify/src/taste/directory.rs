//! The playlist directory has its own bounded, durable read. A track checkpoint
//! cannot help a playlist that the directory's first twenty pages never named.
//! Spotify gives this collection no snapshot ID: resume only after rereading
//! its head and last accepted page, checking identities and raw coverage. A
//! changed boundary restarts the scan; only a complete scan can prune proofs.

use super::continuation::Scan;
use super::paging::Observation;
use super::{MAX_PAGES, PlaylistObservation, Reader};
use crate::SpotifyError;
use crate::client::{PAGE_LIMIT, SimplifiedPlaylist, random_state};
use fsonos_core::store::{
    MAX_PLAYLIST_BYTES, MAX_PLAYLIST_DIRECTORY_ENTRIES, SpotifyLibraryOwner,
    SpotifyPlaylistPublicationFence, Store, StoreError,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::Display;
use std::ops::Range;

#[derive(Default)]
pub(super) struct Directory {
    pub(super) items: Vec<SimplifiedPlaylist>,
    pub(super) complete: bool,
    pub(super) limited: bool,
    pub(super) receipt: Option<DirectoryReceipt>,
}

#[derive(Debug, Clone)]
pub(crate) struct DirectoryReceipt {
    encoded: String,
    /// Only the full pass using a completed background scan can consume it.
    pub(super) reconcile: bool,
}

#[derive(Clone, Serialize, Deserialize)]
struct Progress {
    version: u8,
    owner_id: String,
    generation: u64,
    examined: u32,
    source_total: Option<u32>,
    first_end: u32,
    last_start: u32,
    complete: bool,
    limited: bool,
    #[serde(default)]
    reconcile: bool,
    /// Every public acceptance changes the directory receipt, including a
    /// bounded subset whose Spotify snapshot IDs themselves did not change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    publication: Option<String>,
    items: Vec<SimplifiedPlaylist>,
}

impl Progress {
    fn new(owner: &str, generation: u64) -> Self {
        Self {
            version: 1,
            owner_id: owner.to_owned(),
            generation,
            examined: 0,
            source_total: None,
            first_end: 0,
            last_start: 0,
            complete: false,
            limited: false,
            reconcile: false,
            publication: None,
            items: Vec::new(),
        }
    }

    fn valid(&self, owner: &str, generation: u64) -> bool {
        self.version == 1
            && self.owner_id == owner
            && self.generation == generation
            && self.examined <= MAX_PLAYLIST_DIRECTORY_ENTRIES
            && usize::try_from(self.examined).ok() == Some(self.items.len())
            && self.first_end <= PAGE_LIMIT.min(self.examined)
            && (self.examined == 0 || self.first_end > 0)
            && self.last_start <= self.examined
            && (self.examined == 0 || self.last_start < self.examined)
            && self.examined - self.last_start <= PAGE_LIMIT
            && !(self.complete && self.limited)
            && self.publication.as_ref().is_none_or(|publication| {
                !publication.is_empty()
                    && publication.len() <= 128
                    && !publication.chars().any(char::is_control)
            })
            && self.source_total.is_none_or(|total| {
                self.examined <= total && (!self.complete || self.examined == total)
            })
            && valid_items(&self.items)
    }

    fn boundary(&self, page: &Observation<SimplifiedPlaylist>, first: u32, end: u32) -> bool {
        page.examined == end
            && page.complete == (self.complete && end == self.examined)
            && self
                .source_total
                .zip(page.source_total)
                .is_none_or(|(before, after)| before == after)
            && self.items[first as usize..end as usize]
                .iter()
                .zip(&page.items)
                .all(|(before, after)| {
                    before.id == after.id
                        && before.collaborative == after.collaborative
                        && before.owner.as_ref().map(|owner| &owner.id)
                            == after.owner.as_ref().map(|owner| &owner.id)
                })
    }

    fn append(&mut self, page: Observation<SimplifiedPlaylist>) {
        self.last_start = self.examined;
        self.examined = page.examined;
        self.source_total = page.source_total;
        if self.items.is_empty() {
            self.first_end = self.examined;
        }
        self.items.extend(page.items);
        self.complete = page.complete;
    }

    fn result(mut self, accepted: bool, stale: Range<usize>) -> Directory {
        // A directory row retained from an earlier pass cannot supply today's
        // playlist version. Pending track windows must fetch their version
        // before resuming; otherwise a changed middle-page playlist would
        // keep retrying the old revision until the next full library refresh.
        for (position, item) in self.items.iter_mut().enumerate() {
            if stale.contains(&position) {
                item.snapshot_id = None;
            }
        }
        Directory {
            items: self.items,
            complete: self.complete && accepted,
            limited: self.limited,
            receipt: None,
        }
    }

    fn limit(&mut self) -> bool {
        if self.examined <= MAX_PLAYLIST_DIRECTORY_ENTRIES
            && serde_json::to_string(self).is_ok_and(|value| value.len() <= MAX_PLAYLIST_BYTES)
        {
            return false;
        }
        // A terminal marker avoids spending the same background budget on an
        // oversized directory forever. Public names and contents survive.
        self.items.clear();
        self.examined = 0;
        self.first_end = 0;
        self.last_start = 0;
        self.complete = false;
        self.limited = true;
        self.reconcile = false;
        true
    }
}

/// Build the exact read fence, accepted versions and acknowledgements without
/// modifying durable state. The store commits them with the public projection.
/// A fallback captured before taste/cache reads fences even a failed directory
/// read against a concurrent background publication.
pub(crate) fn prepare_publication(
    owner: &SpotifyLibraryOwner,
    receipt: Option<&DirectoryReceipt>,
    fallback: Option<&str>,
    observations: &[PlaylistObservation],
) -> Result<(SpotifyPlaylistPublicationFence, Option<DirectoryReceipt>), SpotifyError> {
    let expected = receipt.map(|receipt| receipt.encoded.as_str()).or(fallback);
    let updates = observations
        .iter()
        .map(|observation| super::playlist_scan::publication(owner, observation))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect();
    let mut fence = SpotifyPlaylistPublicationFence {
        expected_directory: expected.map(str::to_owned),
        directory: None,
        observations: updates,
    };
    let Some(mut progress) = expected
        .and_then(|encoded| serde_json::from_str::<Progress>(encoded).ok())
        .filter(|progress| progress.valid(&owner.owner_id, owner.generation))
    else {
        return Ok((fence, None));
    };
    let versions: HashMap<_, _> = observations
        .iter()
        .filter(|observation| !observation.failed && observation.progress.is_some())
        .filter_map(|observation| {
            Some((
                observation
                    .contents
                    .playlist_uri
                    .strip_prefix("spotify:playlist:")?,
                observation.snapshot_id.as_str(),
            ))
        })
        .collect();
    for item in &mut progress.items {
        if let Some(version) = versions.get(item.id.as_str())
            && item.snapshot_id.as_deref() != Some(*version)
        {
            item.snapshot_id = Some((*version).to_owned());
        }
    }
    progress.publication = Some(random_state()?);
    let encoded =
        serde_json::to_string(&progress).map_err(|error| StoreError::Backend(error.to_string()))?;
    if encoded.len() > MAX_PLAYLIST_BYTES {
        return Err(SpotifyError::Decode(
            "playlist publication exceeds its 16 MiB directory limit".into(),
        ));
    }
    fence.directory = Some(encoded.clone());
    Ok((
        fence,
        Some(DirectoryReceipt {
            encoded,
            reconcile: receipt.is_some_and(|receipt| receipt.reconcile),
        }),
    ))
}

/// Called only after an account-fenced full publication and a complete union.
/// Cancellation, essential-read failure or incomplete taste leaves the request
/// durable; even an account switch away and back prevents acknowledgement.
pub(crate) fn finish_reconciliation<S: Store + ?Sized>(
    store: &mut S,
    owner: &SpotifyLibraryOwner,
    receipt: Option<&DirectoryReceipt>,
) -> Result<(), StoreError> {
    let Some(receipt) = receipt.filter(|receipt| receipt.reconcile) else {
        return Ok(());
    };
    let mut progress: Progress = serde_json::from_str(&receipt.encoded)
        .map_err(|error| StoreError::Backend(error.to_string()))?;
    if !progress.valid(&owner.owner_id, owner.generation) || !progress.complete {
        return Err(StoreError::Backend(
            "invalid playlist directory reconciliation receipt".into(),
        ));
    }
    progress.reconcile = false;
    let applied =
        serde_json::to_string(&progress).map_err(|error| StoreError::Backend(error.to_string()))?;
    store.save_spotify_playlist_directory(owner, Some(&receipt.encoded), &applied, None)?;
    Ok(())
}

fn valid_items(items: &[SimplifiedPlaylist]) -> bool {
    items.iter().all(|item| {
        !item.id.is_empty()
            && item.id.len() <= 128
            && item.id.bytes().all(|byte| byte.is_ascii_alphanumeric())
            && item.name.len() <= 16 * 1024
            && item.owner.as_ref().is_none_or(|owner| {
                owner.id.len() <= 1_024 && !owner.id.chars().any(char::is_control)
            })
            && item.snapshot_id.as_ref().is_none_or(|snapshot| {
                snapshot.len() <= 1_024 && !snapshot.chars().any(char::is_control)
            })
    })
}

impl Reader<'_> {
    fn directory_failed(&mut self, reason: impl Display) {
        self.read
            .failed
            .push(("playlist directory".into(), reason.to_string()));
    }

    async fn directory_page(
        &mut self,
        offset: u32,
        total: Option<u32>,
    ) -> Option<Observation<SimplifiedPlaylist>> {
        let url = self.session.endpoints().my_playlists(offset);
        let page = self
            .observe_window_from(
                &url,
                "playlists",
                1,
                Some(PAGE_LIMIT as usize),
                (offset, total),
                None,
            )
            .await;
        if page.failed {
            return None;
        }
        if !valid_items(&page.items) {
            self.directory_failed("invalid playlist directory entry");
            return None;
        }
        Some(page)
    }

    fn save_directory<S: Store + ?Sized>(
        &mut self,
        store: &mut S,
        receipt: Option<&SpotifyLibraryOwner>,
        expected: &mut Option<String>,
        progress: &Progress,
    ) -> bool {
        let encoded = match serde_json::to_string(progress) {
            Ok(encoded) if encoded.len() <= MAX_PLAYLIST_BYTES => encoded,
            _ => {
                self.directory_failed("playlist directory exceeds its 16 MiB metadata limit");
                return false;
            }
        };
        let active = progress.complete.then(|| {
            progress
                .items
                .iter()
                .filter(|item| item.readable_by(&progress.owner_id))
                .map(|item| format!("spotify:playlist:{}", item.id))
                .collect::<Vec<_>>()
        });
        let saved = if let Some(receipt) = receipt {
            store.save_spotify_playlist_directory(
                receipt,
                expected.as_deref(),
                &encoded,
                active.as_deref(),
            )
        } else {
            // A standalone taste read cannot adopt an unproved library owner.
            // Its directory remains ephemeral and cannot prune durable proofs.
            Ok(true)
        };
        match saved {
            Ok(true) => {
                *expected = Some(encoded);
                true
            }
            Ok(false) => {
                if let Ok(current) = store.spotify_library_owner()
                    && current.as_ref() != receipt
                {
                    self.read.continuation.owner = current.map(|owner| owner.owner_id);
                    self.read.continuation.requires_refresh = true;
                }
                self.directory_failed(
                    "the account or playlist directory changed during this pass; retry from its accepted checkpoint",
                );
                false
            }
            Err(error) => {
                self.directory_failed(error);
                false
            }
        }
    }

    /// Full syncs refresh from the head, except for the one reconciliation owed
    /// to a completed background directory. Background passes resume their
    /// checkpoint and reuse a complete directory while finishing its items.
    /// Head/boundary metadata is refreshed on reuse; a full sync refreshes every
    /// directory row, including changes within an already complete middle page.
    pub(super) async fn playlist_directory<S: Store + ?Sized>(
        &mut self,
        store: &mut S,
        owner: &str,
        scan: Scan<'_>,
    ) -> Directory {
        let current = match store.spotify_library_owner() {
            Ok(current) => current,
            Err(error) => {
                self.directory_failed(error);
                return Directory::default();
            }
        };
        let receipt = self.receipt.cloned().or_else(|| current.clone());
        if receipt
            .as_ref()
            .is_some_and(|receipt| receipt.owner_id != owner || current.as_ref() != Some(receipt))
        {
            self.read.continuation.requires_refresh = true;
            self.read.continuation.owner = current.map(|owner| owner.owner_id);
            self.directory_failed(
                "the library's Spotify owner changed; refresh it before continuing",
            );
            return Directory::default();
        }
        let generation = receipt.as_ref().map_or(0, |receipt| receipt.generation);
        let mut encoded = match receipt
            .as_ref()
            .map(|receipt| store.spotify_playlist_directory(receipt))
            .transpose()
        {
            Ok(encoded) => encoded.flatten(),
            Err(error) => {
                self.directory_failed(error);
                return Directory::default();
            }
        };
        let previous = encoded
            .as_deref()
            .and_then(|encoded| serde_json::from_str::<Progress>(encoded).ok())
            .filter(|progress| progress.valid(owner, generation));
        if matches!(scan, Scan::Pending(_))
            && let Some(previous) = previous.as_ref().filter(|previous| previous.limited)
        {
            self.directory_failed(
                "playlist directory is at its cache limit; a full refresh may retry it",
            );
            return previous.clone().result(false, 0..usize::MAX);
        }
        let Some(head) = self.directory_page(0, None).await else {
            return previous.map_or_else(Directory::default, |progress| {
                progress.result(false, 0..usize::MAX)
            });
        };
        let mut requests = 1;
        let mut progress = Progress::new(owner, generation);
        progress.reconcile = matches!(scan, Scan::Pending(_));
        let mut resumed = false;
        let mut stale = 0..0;
        let reconciliation = matches!(scan, Scan::Refresh)
            && previous
                .as_ref()
                .is_some_and(|progress| progress.complete && progress.reconcile);
        if (matches!(scan, Scan::Pending(_)) || reconciliation)
            && let Some(mut candidate) = previous.clone()
            && candidate.boundary(&head, 0, candidate.first_end)
        {
            candidate.source_total = head.source_total.or(candidate.source_total);
            let boundary = if candidate.last_start > 0 {
                requests += 1;
                let Some(boundary) = self.directory_page(candidate.last_start, None).await else {
                    return candidate.result(false, 0..usize::MAX);
                };
                Some(boundary)
            } else {
                None
            };
            if boundary.as_ref().is_none_or(|boundary| {
                candidate.boundary(boundary, candidate.last_start, candidate.examined)
            }) {
                candidate.items[..head.items.len()].clone_from_slice(&head.items);
                if let Some(boundary) = boundary {
                    candidate.items[candidate.last_start as usize..]
                        .clone_from_slice(&boundary.items);
                }
                stale = candidate.first_end as usize
                    ..candidate.last_start.max(candidate.first_end) as usize;
                candidate.reconcile |= matches!(scan, Scan::Pending(_)) && !candidate.complete;
                progress = candidate;
                resumed = true;
            }
        }
        if resumed {
            let first = progress.examined;
            if !self.accept_directory_page(
                store,
                receipt.as_ref(),
                &mut encoded,
                &mut progress,
                first,
            ) {
                return Directory::default();
            }
        } else {
            progress.append(head);
            if !self.accept_directory_page(store, receipt.as_ref(), &mut encoded, &mut progress, 0)
            {
                return previous.map_or_else(Directory::default, |progress| {
                    progress.result(false, 0..usize::MAX)
                });
            }
        }
        while !progress.complete && !progress.limited && requests < MAX_PAGES {
            requests += 1;
            let Some(page) = self
                .directory_page(progress.examined, progress.source_total)
                .await
            else {
                break;
            };
            let first = progress.examined;
            let mut next = progress.clone();
            next.append(page);
            if !self.accept_directory_page(store, receipt.as_ref(), &mut encoded, &mut next, first)
            {
                break;
            }
            progress = next;
        }
        if !progress.complete && !progress.limited {
            self.directory_failed(format!(
                "playlist directory checked through position {}; continuing from that checkpoint",
                progress.examined,
            ));
        }
        let reconciling = reconciliation && resumed && progress.complete;
        let mut result = progress.result(true, if reconciling { 0..0 } else { stale });
        if receipt.is_some() {
            result.receipt = encoded.map(|encoded| DirectoryReceipt {
                encoded,
                reconcile: reconciling,
            });
        }
        result
    }

    fn accept_directory_page<S: Store + ?Sized>(
        &mut self,
        store: &mut S,
        receipt: Option<&SpotifyLibraryOwner>,
        encoded: &mut Option<String>,
        progress: &mut Progress,
        first: u32,
    ) -> bool {
        if progress.limit() {
            self.directory_failed("playlist directory reached its entry or 16 MiB metadata limit");
        }
        if !self.save_directory(store, receipt, encoded, progress) {
            return false;
        }
        self.read.continuation.advanced = self
            .read
            .continuation
            .advanced
            .saturating_add(progress.examined.saturating_sub(first));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: String) -> SimplifiedPlaylist {
        SimplifiedPlaylist {
            id: "DirectoryItem".into(),
            name,
            owner: None,
            collaborative: false,
            snapshot_id: None,
        }
    }

    #[test]
    fn directory_bounds_make_small_terminal_checkpoints() {
        for metadata in [false, true] {
            let mut progress = Progress::new("owner", 3);
            if metadata {
                progress.items = vec![item("x".repeat(16 * 1024)); 1_024];
                progress.examined = 1_024;
            } else {
                progress.examined = MAX_PLAYLIST_DIRECTORY_ENTRIES + 1;
            }
            progress.source_total = Some(progress.examined + 1);
            progress.first_end = PAGE_LIMIT;
            progress.last_start = progress.examined - PAGE_LIMIT;
            assert!(progress.limit());
            assert!(progress.limited);
            assert!(!progress.complete);
            assert!(progress.items.is_empty());
            assert!(progress.valid("owner", 3));
            assert!(serde_json::to_string(&progress).unwrap().len() < 512);
        }
    }
}
