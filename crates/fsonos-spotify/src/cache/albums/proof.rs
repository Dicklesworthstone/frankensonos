//! Retained album pages are observations, not a catalog snapshot. Resumed
//! scans recheck their root and verify every retained page before publication.
//! Proofs cover only metadata and coverage consumed by whole-work expansion.

use crate::SpotifyError;
use crate::client::{Endpoints, SimplifiedTrack, sha256};
use crate::expand::{MAX_ALBUM_ENTRIES, MAX_ALBUM_PAGES, album_id, album_track};
use crate::library::Page;
use fsonos_core::store::AlbumTrack;
use serde::{Deserialize, Serialize};

const VERSION: u8 = 1;
const MAX_AGE: i64 = 7 * 24 * 60 * 60;
const MAX_BYTES: usize = 8 * 1024 * 1024;
const PROOF_BYTES: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct PartialAlbum {
    pub(super) uri: String,
    pub(super) offset: u32,
    total: Option<u32>,
    pages: usize,
    tracks: Vec<PartialTrack>,
    // Old checkpoints still deserialize, but cannot become verified lists.
    #[serde(default)]
    version: u8,
    #[serde(default)]
    started_at: i64,
    #[serde(default)]
    proofs: Vec<PageProof>,
    #[serde(default)]
    complete: bool,
    #[serde(default)]
    verification_required: bool,
    #[serde(default)]
    checked: usize,
    #[serde(default)]
    retained_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PartialTrack {
    disc_number: u32,
    track_number: u32,
    source_uri: String,
    title: String,
    duration_secs: Option<u32>,
    #[serde(default)]
    explicit: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PageProof {
    offset: u32,
    raw_count: u32,
    total: Option<u32>,
    more: bool,
    item_start: usize,
    item_count: usize,
    digest: [u8; 32],
}

impl From<AlbumTrack> for PartialTrack {
    fn from(track: AlbumTrack) -> Self {
        Self {
            disc_number: track.disc_number,
            track_number: track.track_number,
            source_uri: track.source_uri,
            title: track.title,
            duration_secs: track.duration_secs,
            explicit: track.explicit,
        }
    }
}

impl From<PartialTrack> for AlbumTrack {
    fn from(track: PartialTrack) -> Self {
        Self {
            disc_number: track.disc_number,
            track_number: track.track_number,
            source_uri: track.source_uri,
            title: track.title,
            duration_secs: track.duration_secs,
            explicit: track.explicit,
        }
    }
}

fn invalid(message: &str) -> SpotifyError {
    SpotifyError::Decode(message.into())
}

impl PageProof {
    fn fingerprint(&self, tracks: &[PartialTrack]) -> Result<[u8; 32], SpotifyError> {
        serde_json::to_vec(&(self.offset, self.raw_count, self.total, self.more, tracks))
            .map(|bytes| sha256(&bytes))
            .map_err(|error| invalid(&format!("album page fingerprint: {error}")))
    }

    fn bytes(tracks: &[PartialTrack]) -> Result<usize, SpotifyError> {
        serde_json::to_vec(tracks)
            .map_err(|error| invalid(&format!("album metadata: {error}")))?
            .len()
            .checked_add(PROOF_BYTES)
            .ok_or_else(|| invalid("album metadata size overflow"))
    }
}

impl PartialAlbum {
    pub(super) fn new(uri: String, at: i64) -> Self {
        let retained_bytes = uri.len().saturating_add(PROOF_BYTES);
        Self {
            uri,
            offset: 0,
            total: None,
            pages: 0,
            tracks: Vec::new(),
            version: VERSION,
            started_at: at,
            proofs: Vec::new(),
            complete: false,
            verification_required: false,
            checked: 0,
            retained_bytes,
        }
    }

    /// The private payload must agree with its proofs before any cursor is
    /// trusted. This full validation happens once per pass; intake hashes and
    /// measures only the new page, avoiding repeated whole-album copies.
    pub(super) fn resume(&mut self, at: i64) -> bool {
        if !self.valid(at) {
            return false;
        }
        if self.ready() {
            // A completed write that failed, or a crash before publication,
            // cannot carry yesterday's finished verification into today.
            self.checked = 0;
        }
        self.verification_required = !self.proofs.is_empty();
        true
    }

    fn valid(&self, at: i64) -> bool {
        if self.version != VERSION
            || self.started_at <= 0
            || self.started_at > at
            || at.saturating_sub(self.started_at) > MAX_AGE
            || album_id(&self.uri).is_none()
            || self.pages != self.proofs.len()
            || self.pages > MAX_ALBUM_PAGES
            || self.offset as usize > MAX_ALBUM_ENTRIES
            || self
                .total
                .is_some_and(|total| total as usize > MAX_ALBUM_ENTRIES)
            || self.checked > self.pages
            || (!self.verification_required && self.checked != 0)
            || (!self.complete && self.checked > 1)
            || self.retained_bytes > MAX_BYTES
            || self.tracks.iter().any(|track| track.explicit.is_none())
        {
            return false;
        }
        let mut offset = 0u32;
        let mut consumed = 0usize;
        let mut total = None;
        let mut bytes = self.uri.len().saturating_add(PROOF_BYTES);
        for (index, proof) in self.proofs.iter().enumerate() {
            let Some(end) = offset.checked_add(proof.raw_count) else {
                return false;
            };
            let Some(item_end) = consumed.checked_add(proof.item_count) else {
                return false;
            };
            let Some(tracks) = self.tracks.get(consumed..item_end) else {
                return false;
            };
            if proof.offset != offset
                || proof.item_start != consumed
                || proof.item_count > proof.raw_count as usize
                || total.is_some_and(|expected| proof.total != Some(expected))
                || proof.total.is_some_and(|reported| end > reported)
                || (!proof.more && index + 1 != self.pages)
                || (proof.more && proof.raw_count == 0)
                || (!proof.more && proof.total.is_some_and(|reported| end != reported))
                || proof.fingerprint(tracks).ok() != Some(proof.digest)
            {
                return false;
            }
            let Some(next_bytes) = PageProof::bytes(tracks)
                .ok()
                .and_then(|size| bytes.checked_add(size))
            else {
                return false;
            };
            bytes = next_bytes;
            offset = end;
            consumed = item_end;
            total = proof.total;
        }
        offset == self.offset
            && consumed == self.tracks.len()
            && total == self.total
            && self.complete == self.proofs.last().is_some_and(|proof| !proof.more)
            && bytes == self.retained_bytes
    }

    pub(super) fn has_pages(&self) -> bool {
        !self.proofs.is_empty()
    }

    pub(super) fn ready(&self) -> bool {
        self.complete && (!self.verification_required || self.checked == self.pages)
    }

    pub(super) fn verification_index(&self) -> Option<usize> {
        (self.complete && self.verification_required && self.checked < self.pages)
            .then_some(self.checked)
    }

    pub(super) fn page_offset(&self, index: usize) -> Option<u32> {
        self.proofs.get(index).map(|proof| proof.offset)
    }

    pub(super) fn verified_positions(&self) -> usize {
        self.checked
            .checked_sub(1)
            .and_then(|index| self.proofs.get(index))
            .map_or(0, |proof| proof.offset as usize + proof.raw_count as usize)
    }

    pub(super) fn album_tracks(&self) -> Vec<AlbumTrack> {
        self.tracks.iter().cloned().map(AlbumTrack::from).collect()
    }

    /// A validated page is staged before its metadata or coverage is retained.
    pub(super) fn ingest(
        &mut self,
        body: &[u8],
        endpoints: &Endpoints,
    ) -> Result<(), SpotifyError> {
        let id = album_id(&self.uri).ok_or_else(|| invalid("invalid album checkpoint"))?;
        if self.complete || self.pages >= MAX_ALBUM_PAGES {
            return Err(invalid(
                "album page budget exceeded or read already complete",
            ));
        }
        let url = endpoints.album_tracks(id, self.offset);
        let page = Page::<SimplifiedTrack>::read(body, &url, endpoints, self.total, |offset| {
            endpoints.album_tracks(id, offset)
        })?;
        let count =
            u32::try_from(page.items.len()).map_err(|_| invalid("album page is too large"))?;
        let offset = self
            .offset
            .checked_add(count)
            .ok_or_else(|| invalid("album position overflow"))?;
        let tracks: Vec<PartialTrack> = page
            .items
            .iter()
            .filter_map(album_track)
            .map(PartialTrack::from)
            .collect();
        let bytes = self
            .retained_bytes
            .checked_add(PageProof::bytes(&tracks)?)
            .ok_or_else(|| invalid("album metadata size overflow"))?;
        if offset as usize > MAX_ALBUM_ENTRIES
            || page
                .coverage
                .total
                .is_some_and(|total| total as usize > MAX_ALBUM_ENTRIES)
            || bytes > MAX_BYTES
        {
            return Err(invalid("album checkpoint budget exceeded"));
        }
        let mut proof = PageProof {
            offset: self.offset,
            raw_count: count,
            total: page.coverage.total,
            more: page.next.is_some(),
            item_start: self.tracks.len(),
            item_count: tracks.len(),
            digest: [0; 32],
        };
        proof.digest = proof.fingerprint(&tracks)?;
        self.offset = offset;
        self.total = proof.total;
        self.complete = !proof.more;
        self.pages += 1;
        self.retained_bytes = bytes;
        self.proofs.push(proof);
        self.tracks.extend(tracks);
        Ok(())
    }

    /// A changed count is a new catalog observation even if this old offset
    /// no longer exists. Other malformed pages remain errors, not evidence
    /// that a partial album is complete.
    pub(super) fn verify(
        &mut self,
        index: usize,
        body: &[u8],
        endpoints: &Endpoints,
    ) -> Result<bool, SpotifyError> {
        let proof = self
            .proofs
            .get(index)
            .ok_or_else(|| invalid("unknown retained album page"))?;
        let reported: Page<SimplifiedTrack> = serde_json::from_slice(body)
            .map_err(|error| invalid(&format!("album verification page: {error}")))?;
        if let (Some(expected), Some(found)) = (self.total, reported.coverage.total)
            && expected != found
        {
            return Ok(false);
        }
        let id = album_id(&self.uri).ok_or_else(|| invalid("invalid album checkpoint"))?;
        let url = endpoints.album_tracks(id, proof.offset);
        let page = Page::<SimplifiedTrack>::read(body, &url, endpoints, proof.total, |offset| {
            endpoints.album_tracks(id, offset)
        })?;
        let tracks: Vec<PartialTrack> = page
            .items
            .iter()
            .filter_map(album_track)
            .map(PartialTrack::from)
            .collect();
        let observed = PageProof {
            offset: proof.offset,
            raw_count: u32::try_from(page.items.len())
                .map_err(|_| invalid("album verification page is too large"))?,
            total: page.coverage.total,
            more: page.next.is_some(),
            item_start: proof.item_start,
            item_count: tracks.len(),
            digest: [0; 32],
        };
        if observed.fingerprint(&tracks)? != proof.digest {
            return Ok(false);
        }
        if index == self.checked {
            self.checked += 1;
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests;
