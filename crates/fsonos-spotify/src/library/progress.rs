//! Compact saved-library progress, independent of its durable store.
//! A resumed offset scan is checked against its root pages and then against
//! every retained page before it can replace the public library. Spotify does
//! not provide an atomic snapshot for these collections.

use super::intake::{ITEM_BUDGET, PAGE_BUDGET, check_route, invalid, offset, path};
use super::{Fetch, LibraryItem, LibraryRead, Origin};
use crate::SpotifyError;
use crate::client::{Album, Endpoints, random_state, sha256};
use fsonos_core::store::{MAX_SPOTIFY_LIBRARY_PROGRESS_BYTES, SpotifyLibraryOwner};
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};

const VERSION: u8 = 1;
const MAX_AGE_SECS: i64 = 7 * 24 * 60 * 60;
const MAX_FIELD: usize = 16 * 1024;

/// The essential read's current phase, before the full refresh is accepted.
/// `Ready` means saved music is prepared; it does not claim publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SavedLibraryPhase {
    Reading,
    Verifying,
    Ready,
}

/// Compact evidence from a successfully persisted essential-read checkpoint.
/// The daemon can retain this beside its retry deadline without parsing or
/// exposing the private library payload on every status request. Counts refer
/// to the last saved checkpoint, not to a percentage or an atomic API snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedLibraryProgress {
    /// The verified account being read, which may differ from the public cache.
    pub owner: String,
    /// The public ownership receipt that fenced this saved checkpoint.
    pub base_owner: Option<String>,
    pub base_generation: Option<u64>,
    /// Validated input pages retained; verification requests do not add pages.
    pub pages: usize,
    /// Playable input rows retained, before deduplicating saves and likes.
    pub tracks: usize,
    /// Retained pages checked again after an interrupted read.
    pub verified_pages: usize,
    pub phase: SavedLibraryPhase,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct LibraryProgress {
    version: u8,
    owner_id: String,
    api: String,
    started_at: i64,
    nonce: String,
    read: LibraryRead,
    proofs: Vec<PageProof>,
    verification_required: bool,
    checked: usize,
    ready: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PageProof {
    fetch: Fetch,
    spawned: Vec<Fetch>,
    item_start: usize,
    item_count: usize,
    raw_count: u32,
    total: Option<u32>,
    digest: [u8; 32],
}

#[derive(Deserialize)]
struct RawPage {
    items: Vec<serde::de::IgnoredAny>,
    #[serde(default)]
    total: Option<u32>,
}

impl Fetch {
    fn total(&self) -> Option<u32> {
        match self {
            Self::SavedAlbums(_, total)
            | Self::SavedTracks(_, total)
            | Self::AlbumTracks(_, _, _, total) => *total,
        }
    }

    fn canonical(&self, endpoints: &Endpoints) -> Result<Self, SpotifyError> {
        let position = offset(self.url())?;
        Ok(match self {
            Self::SavedAlbums(_, total) => {
                Self::SavedAlbums(endpoints.saved_albums(position), *total)
            }
            Self::SavedTracks(_, total) => {
                Self::SavedTracks(endpoints.saved_tracks(position), *total)
            }
            Self::AlbumTracks(_, album, added, total) => {
                let mut album = album.clone();
                // This catalog field does not enter LibraryItem or hydration.
                album.album_type = None;
                for artist in &mut album.artists {
                    artist.id = None;
                    artist.uri = None;
                }
                Self::AlbumTracks(
                    endpoints.album_tracks(&album.id, position),
                    album,
                    *added,
                    *total,
                )
            }
        })
    }
}

fn fingerprint(
    endpoints: &Endpoints,
    raw_count: u32,
    total: Option<u32>,
    items: &[LibraryItem],
    spawned: &[Fetch],
) -> Result<[u8; 32], SpotifyError> {
    let next = spawned
        .iter()
        .map(|fetch| fetch.canonical(endpoints))
        .collect::<Result<Vec<_>, _>>()?;
    let meaningful = serde_json::to_vec(&(raw_count, total, items, next))
        .map_err(|error| invalid(&format!("library page proof: {error}")))?;
    Ok(sha256(&meaningful))
}

// An inherited count changing is evidence of a new collection, not a
// permanently malformed continuation of the old one. Other decode failures
// still leave the accepted prefix available for a corrected retry.
fn changed_total(expected: Option<u32>, body: &[u8]) -> bool {
    expected.is_some_and(|expected| {
        serde_json::from_slice::<RawPage>(body)
            .ok()
            .and_then(|page| page.total)
            .is_some_and(|observed| observed != expected)
    })
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

fn valid_item(item: &LibraryItem) -> bool {
    item.source_uri
        .strip_prefix("spotify:track:")
        .is_some_and(valid_id)
        && matches!(item.origin, Origin::SavedAlbum | Origin::LikedTrack)
        && item.taste_pm.is_none()
        && item.artist_id.as_deref().is_none_or(|id| id.len() <= 128)
        && std::iter::once(item.title.as_str())
            .chain(item.artists.iter().map(String::as_str))
            .chain(item.album.as_deref())
            .chain(item.album_uri.as_deref())
            .chain(item.album_artists.iter().map(String::as_str))
            .chain(item.genres.iter().map(String::as_str))
            .chain(item.label.as_deref())
            .all(|field| field.len() <= MAX_FIELD)
}

fn valid_album(album: &Album) -> bool {
    valid_id(&album.id)
        && album.uri == format!("spotify:album:{}", album.id)
        && album.tracks.is_none()
        && std::iter::once(album.name.as_str())
            .chain(album.genres.iter().map(String::as_str))
            .chain(album.label.as_deref())
            .chain(album.release_date.as_deref())
            .all(|field| field.len() <= MAX_FIELD)
        && album.artists.iter().all(|artist| {
            artist.name.len() <= MAX_FIELD
                && artist.id.as_ref().is_none_or(|id| id.len() <= 128)
                && artist.uri.as_ref().is_none_or(|uri| uri.len() <= MAX_FIELD)
        })
}

fn valid_fetch(endpoints: &Endpoints, fetch: &Fetch) -> bool {
    fetch.url().len() <= 4096
        && check_route(endpoints, fetch).is_ok()
        && offset(fetch.url()).is_ok_and(|at| fetch.total().is_none_or(|total| at <= total))
        && match fetch {
            Fetch::AlbumTracks(_, album, _, _) => valid_album(album),
            _ => true,
        }
}

impl LibraryProgress {
    pub(crate) fn new(owner_id: &str, endpoints: &Endpoints, now: i64) -> Self {
        Self {
            version: VERSION,
            owner_id: owner_id.to_owned(),
            api: endpoints.api.clone(),
            started_at: now,
            nonce: String::new(),
            read: LibraryRead::new(endpoints),
            proofs: Vec::new(),
            verification_required: false,
            checked: 0,
            ready: false,
        }
    }

    pub(crate) fn restore(
        encoded: &str,
        owner_id: &str,
        endpoints: &Endpoints,
        now: i64,
    ) -> Option<Self> {
        if encoded.len() > MAX_SPOTIFY_LIBRARY_PROGRESS_BYTES {
            return None;
        }
        let mut progress: Self = serde_json::from_str(encoded).ok()?;
        progress.read.endpoints = endpoints.clone();
        progress.valid(owner_id, endpoints, now).then_some(progress)
    }

    /// A fresh nonce fences even competing consumers of a completed read.
    pub(crate) fn claim(&mut self) -> Result<(), SpotifyError> {
        self.nonce = random_state()?;
        if self.pages() > 0 {
            self.verification_required = true;
            if self.ready {
                // Publication may have failed after adoption. Retain the work,
                // but do not treat an older complete proof as a fresh read.
                self.checked = 0;
                self.ready = false;
            }
        }
        Ok(())
    }

    pub(crate) fn encode(&self) -> Result<String, SpotifyError> {
        let encoded = serde_json::to_string(self)
            .map_err(|error| invalid(&format!("library checkpoint: {error}")))?;
        if encoded.len() > MAX_SPOTIFY_LIBRARY_PROGRESS_BYTES {
            return Err(invalid(
                "saved-library progress exceeds its 64 MiB metadata limit",
            ));
        }
        Ok(encoded)
    }

    pub(crate) fn pages(&self) -> usize {
        self.read.pages
    }

    pub(crate) fn tracks(&self) -> usize {
        self.read.items.len()
    }

    pub(crate) fn summary(&self, base: Option<&SpotifyLibraryOwner>) -> SavedLibraryProgress {
        SavedLibraryProgress {
            owner: self.owner_id.clone(),
            base_owner: base.map(|owner| owner.owner_id.clone()),
            base_generation: base.map(|owner| owner.generation),
            pages: self.pages(),
            tracks: self.tracks(),
            verified_pages: self.checked,
            phase: if self.next_url().is_some() {
                SavedLibraryPhase::Reading
            } else if self.next_check().is_some() {
                SavedLibraryPhase::Verifying
            } else {
                SavedLibraryPhase::Ready
            },
        }
    }

    pub(crate) fn next_url(&self) -> Option<&str> {
        self.read.next_url()
    }

    pub(crate) fn proof_url(&self, index: usize) -> &str {
        self.proofs[index].fetch.url()
    }

    pub(crate) fn roots(&self) -> Vec<usize> {
        self.proofs
            .iter()
            .enumerate()
            .filter(|(_, proof)| {
                matches!(
                    proof.fetch,
                    Fetch::SavedAlbums(_, _) | Fetch::SavedTracks(_, _)
                ) && offset(proof.fetch.url()).is_ok_and(|position| position == 0)
            })
            .map(|(index, _)| index)
            .collect()
    }

    pub(crate) fn collection_changed(&self, body: &[u8]) -> bool {
        self.read
            .pending
            .front()
            .is_some_and(|fetch| changed_total(fetch.total(), body))
    }

    /// Keep the same atomic intake rules as the unbounded public reader.
    pub(crate) fn ingest(&mut self, body: &[u8]) -> Result<(), SpotifyError> {
        let fetch = self
            .read
            .pending
            .front()
            .cloned()
            .ok_or_else(|| invalid("library read already complete"))?;
        let raw: RawPage = serde_json::from_slice(body)
            .map_err(|error| invalid(&format!("library page: {error}")))?;
        let raw_count = u32::try_from(raw.items.len())
            .map_err(|_| invalid("library page has too many raw entries"))?;
        let total = raw.total.or(fetch.total());
        let item_start = self.read.items.len();
        let previous_pending = self.read.pending.len();
        self.read.ingest(body)?;
        let spawned: Vec<_> = self
            .read
            .pending
            .iter()
            .skip(previous_pending - 1)
            .cloned()
            .collect();
        let items = &self.read.items[item_start..];
        let digest = fingerprint(&self.read.endpoints, raw_count, total, items, &spawned)?;
        self.proofs.push(PageProof {
            fetch,
            spawned,
            item_start,
            item_count: items.len(),
            raw_count,
            total,
            digest,
        });
        self.checked = 0;
        self.ready = false;
        Ok(())
    }

    /// Compare only metadata used by intake, preserving raw coverage. Unknown
    /// response fields and JSON object key order are deliberately irrelevant.
    pub(crate) fn matches(&self, index: usize, body: &[u8]) -> Result<bool, SpotifyError> {
        let proof = &self.proofs[index];
        if changed_total(proof.total, body) {
            return Ok(false);
        }
        let mut page = Self::new(&self.owner_id, &self.read.endpoints, self.started_at);
        page.read.pending = VecDeque::from([proof.fetch.clone()]);
        page.read.requested = HashSet::from([proof.fetch.url().to_owned()]);
        page.ingest(body)?;
        let observed = &page.proofs[0];
        Ok(observed.digest == proof.digest
            && observed.item_count == proof.item_count
            && page.read.items
                == self.read.items[proof.item_start..proof.item_start + proof.item_count])
    }

    pub(crate) fn next_check(&self) -> Option<usize> {
        (self.read.pending.is_empty()
            && self.verification_required
            && self.checked < self.proofs.len())
        .then_some(self.checked)
    }

    pub(crate) fn checked(&mut self) {
        self.checked += 1;
    }

    pub(crate) fn finish(&mut self) -> bool {
        if self.next_url().is_some() || self.next_check().is_some() {
            return false;
        }
        self.ready = true;
        true
    }

    pub(crate) fn into_items(self) -> Vec<LibraryItem> {
        self.read.into_items()
    }

    fn valid(&self, owner: &str, endpoints: &Endpoints, now: i64) -> bool {
        if self.version != VERSION
            || self.owner_id != owner
            || self.api != endpoints.api
            || self.started_at < 0
            || self.started_at > now.saturating_add(300)
            || now.saturating_sub(self.started_at) >= MAX_AGE_SECS
            || self.nonce.is_empty()
            || self.nonce.len() > 128
            || self.read.pages > PAGE_BUDGET
            || self.read.items.len() > ITEM_BUDGET
            || self.read.requested.len() > PAGE_BUDGET
            || self.read.pages != self.proofs.len()
            || self.checked > self.proofs.len()
            || (!self.verification_required && self.checked != 0)
            || (!self.read.pending.is_empty() && self.checked != 0)
            || (self.ready
                && (!self.read.pending.is_empty()
                    || (self.verification_required && self.checked != self.proofs.len())))
            || !self.read.items.iter().all(valid_item)
        {
            return false;
        }
        // Replay the accepted queue transitions. A serialized offset is not
        // permission to add, skip, duplicate or redirect an outstanding fetch.
        let mut replay = LibraryRead::new(endpoints);
        let mut item_start = 0usize;
        for proof in &self.proofs {
            if replay.pending.pop_front().as_ref() != Some(&proof.fetch)
                || !valid_fetch(endpoints, &proof.fetch)
                || proof.item_start != item_start
                || proof
                    .fetch
                    .total()
                    .is_some_and(|total| proof.total != Some(total))
            {
                return false;
            }
            let Some(next_item) = item_start.checked_add(proof.item_count) else {
                return false;
            };
            if next_item > self.read.items.len() {
                return false;
            }
            if !fingerprint(
                endpoints,
                proof.raw_count,
                proof.total,
                &self.read.items[item_start..next_item],
                &proof.spawned,
            )
            .is_ok_and(|digest| digest == proof.digest)
            {
                return false;
            }
            item_start = next_item;
            let Some(end) = offset(proof.fetch.url())
                .ok()
                .and_then(|start| start.checked_add(proof.raw_count))
            else {
                return false;
            };
            if proof.total.is_some_and(|total| end > total) {
                return false;
            }
            let mut follows = 0;
            for next in &proof.spawned {
                if !valid_fetch(endpoints, next) || replay.requested.contains(next.url()) {
                    return false;
                }
                if path(next.url()) == path(proof.fetch.url()) {
                    follows += 1;
                    if proof.raw_count == 0
                        || !offset(next.url()).is_ok_and(|position| position == end)
                    {
                        return false;
                    }
                } else if !matches!(proof.fetch, Fetch::SavedAlbums(_, _))
                    || !matches!(next, Fetch::AlbumTracks(..))
                {
                    return false;
                }
                replay.queue(next.clone());
            }
            if follows > 1 || (proof.total.is_some_and(|total| end < total) && follows == 0) {
                return false;
            }
        }
        item_start == self.read.items.len()
            && replay.pending == self.read.pending
            && replay.requested == self.read.requested
            && self
                .read
                .pending
                .iter()
                .all(|fetch| valid_fetch(endpoints, fetch))
    }
}

#[cfg(test)]
mod tests;
