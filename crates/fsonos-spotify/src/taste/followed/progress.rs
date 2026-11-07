//! Private cursor evidence; only a fully checked directory can retire taste.

use super::{FollowedArtistContinuation, FollowedArtistPhase};
use crate::SpotifyError;
use crate::client::{Artist, PAGE_LIMIT, random_state};
use fsonos_core::store::{MAX_FOLLOWED_ARTIST_BYTES, MAX_FOLLOWED_ARTISTS};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

const MAX_AGE: i64 = 7 * 86_400;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Row {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) genres: Vec<String>,
}

impl Row {
    fn valid(&self) -> bool {
        !self.id.is_empty()
            && self.id.len() <= 128
            && self.id.bytes().all(|byte| byte.is_ascii_alphanumeric())
            && !self.name.trim().is_empty()
            && self.name.len() <= 16 * 1024
            && !self.name.chars().any(char::is_control)
            && self.genres.len() <= 256
            && self
                .genres
                .iter()
                .all(|genre| genre.len() <= 1_024 && !genre.chars().any(char::is_control))
    }

    pub(super) fn from_artist(artist: Artist) -> Result<Self, SpotifyError> {
        let row = Self {
            id: artist.id.unwrap_or_default(),
            name: artist.name,
            genres: artist.genres,
        };
        if !row.valid() {
            return Err(invalid("invalid followed artist metadata"));
        }
        Ok(row)
    }

    pub(super) fn artist(&self) -> Artist {
        Artist {
            id: Some(self.id.clone()),
            name: self.name.clone(),
            genres: self.genres.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Page {
    pub(super) after: Option<String>,
    pub(super) next: Option<String>,
    pub(super) total: Option<u32>,
    pub(super) items: Vec<Row>,
}

pub(super) fn cursor(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Progress {
    version: u8,
    pub(super) owner_id: String,
    pub(super) generation: u64,
    created_at: i64,
    nonce: String,
    pub(super) pages: Vec<Page>,
    pub(super) verify: bool,
    pub(super) verified: usize,
    pub(super) applied: bool,
    pub(super) limited: bool,
    limited_pages: usize,
    limited_artists: usize,
}

/// Built once per window; accepted pages update these sets without rescanning
/// the cumulative directory for every newly returned artist.
#[derive(Default)]
pub(super) struct Index {
    artists: HashSet<String>,
    cursors: HashSet<String>,
    next: Option<String>,
    total: Option<u32>,
    pages: usize,
}

fn invalid(detail: &str) -> SpotifyError {
    SpotifyError::Decode(detail.into())
}

impl Index {
    /// This is evidence that the retained prefix no longer describes this
    /// directory. Retrying the same cursor forever cannot repair that prefix.
    pub(super) fn contradicts_prefix(&self, page: &Page) -> bool {
        self.total
            .zip(page.total)
            .is_some_and(|(before, after)| before != after)
            || page.total.is_some_and(|total| {
                self.artists.len().saturating_add(page.items.len()) > total as usize
            })
            || page
                .items
                .iter()
                .any(|item| self.artists.contains(&item.id))
    }

    pub(super) fn accept(&mut self, page: &Page) -> Result<(), SpotifyError> {
        if (self.pages > 0 && self.next.is_none()) || page.after != self.next {
            return Err(invalid(
                "followed artist cursor does not continue its accepted page",
            ));
        }
        if page.items.len() > PAGE_LIMIT as usize
            || page.after.as_deref().is_some_and(|value| !cursor(value))
            || page.next.as_deref().is_some_and(|value| !cursor(value))
            || page.items.iter().any(|item| !item.valid())
        {
            return Err(invalid("invalid followed artist page"));
        }
        if self
            .total
            .zip(page.total)
            .is_some_and(|(before, after)| before != after)
        {
            return Err(invalid(
                "followed artist collection changed total while being read",
            ));
        }
        let total = page.total.or(self.total);
        let end = self.artists.len().saturating_add(page.items.len());
        if total.is_some_and(|total| end > total as usize) {
            return Err(invalid("followed artist page exceeds its advertised total"));
        }
        let mut ids = HashSet::new();
        if page
            .items
            .iter()
            .any(|item| self.artists.contains(&item.id) || !ids.insert(item.id.as_str()))
        {
            return Err(invalid("followed artist collection repeats an artist"));
        }
        if let Some(next) = &page.next {
            if self.cursors.contains(next)
                || page.items.last().map(|item| &item.id) != Some(next)
                || total.is_some_and(|total| end >= total as usize)
            {
                return Err(invalid(
                    "followed artist cursor skips or repeats its returned artists",
                ));
            }
        } else if total.is_some_and(|total| end != total as usize) {
            return Err(invalid(
                "followed artist collection ended before its advertised total",
            ));
        }
        self.artists
            .extend(page.items.iter().map(|item| item.id.clone()));
        if let Some(next) = &page.next {
            self.cursors.insert(next.clone());
        }
        self.next.clone_from(&page.next);
        self.total = total;
        self.pages += 1;
        Ok(())
    }
}

impl Progress {
    pub(super) fn new(owner: &str, generation: u64, now: i64) -> Self {
        Self {
            version: 1,
            owner_id: owner.to_owned(),
            generation,
            created_at: now,
            nonce: String::new(),
            pages: Vec::new(),
            verify: false,
            verified: 0,
            applied: false,
            limited: false,
            limited_pages: 0,
            limited_artists: 0,
        }
    }

    pub(super) fn decode(encoded: &str, owner: &str, generation: u64, now: i64) -> Option<Self> {
        if encoded.len() > MAX_FOLLOWED_ARTIST_BYTES {
            return None;
        }
        let progress: Self = serde_json::from_str(encoded).ok()?;
        (progress.version == 1
            && progress.owner_id == owner
            && progress.generation == generation
            && (0..=MAX_AGE).contains(&now.saturating_sub(progress.created_at))
            && !progress.nonce.is_empty()
            && progress.nonce.len() <= 128
            && !progress.nonce.chars().any(char::is_control)
            && progress.verified <= progress.pages.len()
            && (progress.verify
                || (progress.verified == 0
                    && progress.pages.len() <= super::FOLLOWED_ARTIST_PAGE_BUDGET))
            && (progress.read_complete() || progress.verified == 0)
            && (!progress.applied || progress.ready())
            && (!progress.limited || (progress.pages.is_empty() && !progress.applied))
            && (progress.limited || progress.artists() <= MAX_FOLLOWED_ARTISTS as usize)
            && progress.index().is_ok())
        .then_some(progress)
    }

    /// Even an unchanged completed directory gets a distinct consumer receipt.
    pub(super) fn claim(&mut self) -> Result<(), SpotifyError> {
        self.nonce = random_state()?;
        Ok(())
    }

    pub(super) fn index(&self) -> Result<Index, SpotifyError> {
        let mut index = Index::default();
        for page in &self.pages {
            index.accept(page)?;
        }
        Ok(index)
    }

    pub(super) fn artists(&self) -> usize {
        self.pages.iter().map(|page| page.items.len()).sum()
    }

    pub(super) fn read_complete(&self) -> bool {
        self.pages.last().is_some_and(|page| page.next.is_none())
    }

    pub(super) fn ready(&self) -> bool {
        !self.limited && self.read_complete() && (!self.verify || self.verified == self.pages.len())
    }

    pub(super) fn phase(&self) -> FollowedArtistPhase {
        if self.ready() {
            FollowedArtistPhase::Ready
        } else if self.read_complete() {
            FollowedArtistPhase::Verifying
        } else {
            FollowedArtistPhase::Reading
        }
    }

    pub(super) fn report(&self, durable: bool) -> FollowedArtistContinuation {
        FollowedArtistContinuation {
            owner: Some(self.owner_id.clone()),
            generation: durable.then_some(self.generation),
            pending: !self.limited && !self.applied,
            limited: self.limited,
            pages: if self.limited {
                self.limited_pages
            } else {
                self.pages.len()
            },
            artists: if self.limited {
                self.limited_artists
            } else {
                self.artists()
            },
            verified_pages: if self.ready() {
                self.pages.len()
            } else {
                self.verified
            },
            phase: self.phase(),
            ..FollowedArtistContinuation::default()
        }
    }

    /// Oversized metadata becomes a small terminal marker. A later explicit
    /// full refresh can retry; automatic passes must not repeat it forever.
    pub(super) fn encode(&mut self) -> Result<String, SpotifyError> {
        let mut encoded =
            serde_json::to_string(self).map_err(|error| invalid(&error.to_string()))?;
        if self.artists() > MAX_FOLLOWED_ARTISTS as usize
            || encoded.len() > MAX_FOLLOWED_ARTIST_BYTES
        {
            self.limited_pages = self.pages.len();
            self.limited_artists = self.artists();
            self.pages.clear();
            self.verified = 0;
            self.verify = false;
            self.applied = false;
            self.limited = true;
            encoded = serde_json::to_string(self).map_err(|error| invalid(&error.to_string()))?;
        }
        Ok(encoded)
    }
}

#[cfg(test)]
mod tests;
