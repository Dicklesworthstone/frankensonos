//! Bounded followed-artist discovery, with durable cursor and page evidence.
//! Spotify provides no snapshot token for this directory. Resumed scans
//! recheck their boundaries and every retained page before a full publication
//! may remove old taste; these checks detect changes, not an atomic snapshot.

use super::{ARTIST_PM, MAX_PAGES, Reader, Taste, TasteRead, remember_genres};
use crate::client::{Artist, Me, PAGE_LIMIT};
use crate::session::Session;
use asupersync::Cx;
use fsonos_core::store::{SpotifyLibraryOwner, Store, StoreError};
use serde::{Deserialize, Serialize};
use std::fmt::Display;

mod progress;

use progress::{Index, Page, Progress, Row};

/// Page requests in one full or background pass, including all rechecks.
pub const FOLLOWED_ARTIST_PAGE_BUDGET: usize = MAX_PAGES;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FollowedArtistPhase {
    #[default]
    Reading,
    Verifying,
    /// Membership is checked; the full library union may still be unpublished.
    Ready,
}

/// Compact scheduling evidence; it contains neither artist rows nor cursors.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FollowedArtistContinuation {
    pub owner: Option<String>,
    pub generation: Option<u64>,
    /// Reading, verification or the full publication of a completed directory.
    pub pending: bool,
    /// The directory reached its entry or metadata bound; automatic reads stop.
    pub limited: bool,
    pub pages: usize,
    pub artists: usize,
    pub verified_pages: usize,
    pub phase: FollowedArtistPhase,
    /// Newly retained artists or verified pages, for no-progress backoff.
    pub advanced: u32,
    pub retry_after_secs: u64,
    /// Account ownership changed; no continuation can adopt the new account.
    pub requires_refresh: bool,
}

impl FollowedArtistContinuation {
    #[must_use]
    pub fn refresh_required() -> Self {
        Self {
            requires_refresh: true,
            ..Self::default()
        }
    }

    /// A complete directory still owes one successful full library publication.
    #[must_use]
    pub fn ready(&self) -> bool {
        self.pending
            && !self.limited
            && !self.requires_refresh
            && self.phase == FollowedArtistPhase::Ready
    }
}

#[derive(Debug, Clone)]
pub(crate) struct FollowedReceipt {
    encoded: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    Full,
    Pending,
}

#[derive(Deserialize)]
struct Response {
    artists: ResponsePage,
}

#[derive(Deserialize)]
struct ResponsePage {
    items: Vec<Artist>,
    #[serde(deserialize_with = "Option::deserialize")]
    next: Option<String>,
    total: Option<u32>,
    limit: Option<u32>,
    cursors: Option<ResponseCursor>,
}

#[derive(Deserialize)]
struct ResponseCursor {
    after: Option<String>,
}

struct Scan {
    progress: Progress,
    encoded: Option<String>,
    receipt: Option<SpotifyLibraryOwner>,
    index: Index,
    requests: usize,
    failures: usize,
}

impl Scan {
    fn restart(&mut self, now: i64) -> Result<(), crate::SpotifyError> {
        self.progress = Progress::new(&self.progress.owner_id, self.progress.generation, now);
        self.progress.claim()?;
        self.index = Index::default();
        Ok(())
    }
}

/// Continue membership only. The completed directory feeds artist discovery
/// during the next full union, without adding catalog traffic to this pass.
pub(crate) async fn read_pending_followed<S: Store + ?Sized>(
    session: &mut Session,
    cx: &Cx,
    store: &mut S,
    owner: &SpotifyLibraryOwner,
    now: i64,
) -> (Taste, TasteRead) {
    let mut taste = Taste::default();
    let mut read = TasteRead {
        missing_scopes: session.missing_taste_scopes(),
        ..TasteRead::default()
    };
    if !read.missing_scopes.contains(&"user-follow-read") {
        read.followed_continuation = FollowedArtistContinuation {
            owner: Some(owner.owner_id.clone()),
            generation: Some(owner.generation),
            pending: true,
            ..FollowedArtistContinuation::default()
        };
        let mut reader = Reader {
            session,
            cx,
            read: &mut read,
            owner: None,
            receipt: Some(owner),
        };
        for artist in reader
            .followed(store, &mut taste.followed, now, Mode::Pending)
            .await
        {
            taste.artist(&artist, ARTIST_PM);
            reader.read.followed_artists += 1;
        }
    }
    read.followed_continuation.retry_after_secs = session.retry_after_secs();
    (taste, read)
}

/// Only a successful complete full union can consume this exact membership
/// proof. A newer reader, including one under the same account, keeps its work.
pub(crate) fn finish_followed_reconciliation<S: Store + ?Sized>(
    store: &mut S,
    owner: &SpotifyLibraryOwner,
    receipt: Option<&FollowedReceipt>,
) -> Result<bool, StoreError> {
    let Some(receipt) = receipt else {
        return Ok(false);
    };
    let mut progress: Progress = serde_json::from_str(&receipt.encoded)
        .map_err(|error| StoreError::Backend(error.to_string()))?;
    if progress.owner_id != owner.owner_id
        || progress.generation != owner.generation
        || !progress.ready()
        || progress.applied
    {
        return Ok(false);
    }
    progress.applied = true;
    let encoded = progress
        .encode()
        .map_err(|error| StoreError::Backend(error.to_string()))?;
    store.save_spotify_followed_directory(owner, Some(&receipt.encoded), &encoded)
}

impl Reader<'_> {
    fn followed_failed(&mut self, reason: impl Display) {
        self.read
            .failed
            .push(("followed artists".into(), reason.to_string()));
    }

    fn followed_owner_changed(&mut self, current: Option<SpotifyLibraryOwner>) {
        self.read.followed_continuation = FollowedArtistContinuation {
            owner: current.as_ref().map(|owner| owner.owner_id.clone()),
            generation: current.map(|owner| owner.generation),
            ..FollowedArtistContinuation::refresh_required()
        };
        self.followed_failed("the Spotify account changed; a full library refresh is required");
    }

    async fn followed_page(&mut self, after: Option<&str>) -> Option<Page> {
        let url = after.map_or_else(
            || self.session.endpoints().followed_artists(),
            |after| self.session.endpoints().followed_artists_after(after),
        );
        let response = self.get::<Response>(&url, "followed artists").await?;
        let page = response.artists;
        if page.items.len() > PAGE_LIMIT as usize
            || page.limit.is_some_and(|limit| {
                limit == 0 || limit > PAGE_LIMIT || page.items.len() > limit as usize
            })
        {
            self.followed_failed("followed artist page exceeds its requested item limit");
            return None;
        }
        let next = match page.next {
            Some(url) => match super::paging::followed_cursor(self.session.endpoints(), &url) {
                Ok(cursor) if progress::cursor(&cursor) => Some(cursor),
                Ok(_) => {
                    self.followed_failed("invalid followed artist cursor");
                    return None;
                }
                Err(error) => {
                    self.followed_failed(error);
                    return None;
                }
            },
            None => None,
        };
        if let Some(cursor) = page.cursors.and_then(|cursor| cursor.after)
            && (next.as_ref().is_some_and(|next| *next != cursor)
                || page
                    .items
                    .last()
                    .and_then(|artist| artist.id.as_deref())
                    .or(after)
                    != Some(cursor.as_str())
                || !progress::cursor(&cursor))
        {
            self.followed_failed("followed artist response disagrees with its next cursor");
            return None;
        }
        let items = match page.items.into_iter().map(Row::from_artist).collect() {
            Ok(items) => items,
            Err(error) => {
                self.followed_failed(error);
                return None;
            }
        };
        Some(Page {
            after: after.map(str::to_owned),
            next,
            total: page.total,
            items,
        })
    }

    fn save_followed<S: Store + ?Sized>(&mut self, store: &mut S, scan: &mut Scan) -> bool {
        let encoded = match scan.progress.encode() {
            Ok(encoded) => encoded,
            Err(error) => {
                self.followed_failed(error);
                return false;
            }
        };
        let saved = scan.receipt.as_ref().map_or(Ok(true), |owner| {
            store.save_spotify_followed_directory(owner, scan.encoded.as_deref(), &encoded)
        });
        match saved {
            Ok(true) => {
                let advanced = self.read.followed_continuation.advanced;
                self.read.followed_continuation = scan.progress.report(scan.receipt.is_some());
                self.read.followed_continuation.advanced = advanced;
                scan.encoded = Some(encoded);
                true
            }
            Ok(false) => {
                if let Ok(current) = store.spotify_library_owner()
                    && current.as_ref() != scan.receipt.as_ref()
                {
                    self.followed_owner_changed(current);
                } else {
                    self.followed_failed(
                        "the followed artist checkpoint changed during this pass; retry its accepted cursor",
                    );
                }
                false
            }
            Err(error) => {
                self.followed_failed(error);
                false
            }
        }
    }

    fn remember_followed<S: Store + ?Sized>(&mut self, store: &mut S, page: &Page, now: i64) {
        for row in &page.items {
            remember_genres(store, &row.artist(), now, self.read);
        }
    }

    async fn prepare_followed<S: Store + ?Sized>(
        &mut self,
        store: &mut S,
        now: i64,
        mode: Mode,
    ) -> Option<Scan> {
        let current = match store.spotify_library_owner() {
            Ok(current) => current,
            Err(error) => {
                self.followed_failed(error);
                return None;
            }
        };
        if self.receipt.is_some() && current.as_ref() != self.receipt {
            self.followed_owner_changed(current);
            return None;
        }
        let owner = if let Some(owner) = self.owner {
            owner.to_owned()
        } else {
            self.get::<Me>(&self.session.endpoints().me(), "the user")
                .await?
                .id
        };
        if owner.trim().is_empty() || owner.len() > 1_024 || owner.chars().any(char::is_control) {
            self.followed_failed("invalid Spotify owner identity");
            return None;
        }
        if current
            .as_ref()
            .is_some_and(|current| current.owner_id != owner)
        {
            self.followed_owner_changed(current);
            return None;
        }
        let receipt = self.receipt.cloned().or(current);
        let generation = receipt.as_ref().map_or(0, |owner| owner.generation);
        self.read.followed_continuation = FollowedArtistContinuation {
            owner: Some(owner.clone()),
            generation: receipt.as_ref().map(|owner| owner.generation),
            pending: true,
            ..FollowedArtistContinuation::default()
        };
        let encoded = match receipt
            .as_ref()
            .map(|owner| store.spotify_followed_directory(owner))
            .transpose()
        {
            Ok(encoded) => encoded.flatten(),
            Err(error) => {
                self.followed_failed(error);
                return None;
            }
        };
        let progress = encoded
            .as_deref()
            .and_then(|encoded| Progress::decode(encoded, &owner, generation, now))
            .filter(|progress| mode == Mode::Pending || (!progress.applied && !progress.limited))
            .unwrap_or_else(|| Progress::new(&owner, generation, now));
        self.read.followed_continuation = progress.report(receipt.is_some());
        if mode == Mode::Pending && (progress.limited || progress.applied) {
            return None;
        }
        let mut scan = Scan {
            index: progress.index().unwrap_or_default(),
            progress,
            encoded,
            receipt,
            requests: 0,
            failures: self.read.failed.len(),
        };
        if let Err(error) = scan.progress.claim() {
            self.followed_failed(error);
            return None;
        }
        self.save_followed(store, &mut scan).then_some(scan)
    }

    async fn recheck_followed<S: Store + ?Sized>(
        &mut self,
        store: &mut S,
        scan: &mut Scan,
        now: i64,
    ) -> bool {
        scan.requests += 1;
        let Some(head) = self.followed_page(None).await else {
            return false;
        };
        let mut resumed = scan.progress.pages.first() == Some(&head);
        if resumed && scan.progress.pages.len() > 1 {
            scan.requests += 1;
            let last = scan
                .progress
                .pages
                .last()
                .expect("nonempty followed progress");
            let Some(boundary) = self.followed_page(last.after.as_deref()).await else {
                return false;
            };
            resumed = last == &boundary;
            if resumed {
                self.remember_followed(store, &boundary, now);
            }
        }
        if resumed {
            if !scan.progress.ready() {
                scan.progress.verify = true;
            }
        } else {
            if let Err(error) = scan.restart(now).and_then(|()| scan.index.accept(&head)) {
                self.followed_failed(error);
                return false;
            }
            scan.progress.pages.push(head.clone());
        }
        if !self.save_followed(store, scan) {
            return false;
        }
        self.remember_followed(store, &head, now);
        if !resumed {
            self.read.followed_continuation.advanced =
                u32::try_from(head.items.len()).unwrap_or(u32::MAX);
        }
        true
    }

    fn restart_followed<S: Store + ?Sized>(
        &mut self,
        store: &mut S,
        scan: &mut Scan,
        now: i64,
        reason: &str,
    ) -> bool {
        if let Err(error) = scan.restart(now) {
            self.followed_failed(error);
            return false;
        }
        self.read.followed_continuation.advanced = 0;
        if !self.save_followed(store, scan) {
            return false;
        }
        self.followed_failed(reason);
        true
    }

    async fn advance_followed<S: Store + ?Sized>(
        &mut self,
        store: &mut S,
        scan: &mut Scan,
        now: i64,
    ) -> bool {
        while scan.requests < FOLLOWED_ARTIST_PAGE_BUDGET
            && !scan.progress.ready()
            && !scan.progress.limited
        {
            scan.requests += 1;
            if scan.progress.read_complete() {
                let expected = &scan.progress.pages[scan.progress.verified];
                let Some(page) = self.followed_page(expected.after.as_deref()).await else {
                    break;
                };
                if &page != expected {
                    return self.restart_followed(
                        store,
                        scan,
                        now,
                        "a retained followed artist page changed; restarting its private directory",
                    );
                }
                scan.progress.verified += 1;
                if !self.save_followed(store, scan) {
                    return false;
                }
                self.remember_followed(store, &page, now);
                self.read.followed_continuation.advanced =
                    self.read.followed_continuation.advanced.saturating_add(1);
            } else {
                let after = scan
                    .progress
                    .pages
                    .last()
                    .and_then(|page| page.next.as_deref());
                let Some(page) = self.followed_page(after).await else {
                    break;
                };
                if let Err(error) = scan.index.accept(&page) {
                    self.followed_failed(error);
                    return !scan.index.contradicts_prefix(&page)
                        || self.restart_followed(
                            store,
                            scan,
                            now,
                            "followed artist membership changed; restarting its private directory",
                        );
                }
                let advanced = u32::try_from(page.items.len()).unwrap_or(u32::MAX);
                scan.progress.pages.push(page);
                if !self.save_followed(store, scan) {
                    return false;
                }
                if let Some(page) = scan.progress.pages.last() {
                    self.remember_followed(store, page, now);
                }
                self.read.followed_continuation.advanced = self
                    .read
                    .followed_continuation
                    .advanced
                    .saturating_add(advanced);
            }
        }
        true
    }

    pub(super) async fn followed<S: Store + ?Sized>(
        &mut self,
        store: &mut S,
        proof: &mut Option<FollowedReceipt>,
        now: i64,
        mode: Mode,
    ) -> Vec<Artist> {
        let Some(mut scan) = self.prepare_followed(store, now, mode).await else {
            return Vec::new();
        };
        if !self.recheck_followed(store, &mut scan, now).await
            || !self.advance_followed(store, &mut scan, now).await
        {
            return Vec::new();
        }
        if scan.progress.limited {
            self.followed_failed("followed artists reached their entry or 16 MiB metadata limit; a full refresh may retry");
        } else if !scan.progress.ready()
            && self.read.failed.len() == scan.failures
            && self.read.halted.is_none()
        {
            self.followed_failed(format!(
                "followed artist {:?} retained {} artists across {} pages; continuing from its checkpoint",
                scan.progress.phase(), scan.progress.artists(), scan.progress.pages.len()
            ));
        }
        if scan.receipt.is_some()
            && let Some(encoded) = scan.encoded
        {
            *proof = Some(FollowedReceipt { encoded });
        }
        self.read.followed_continuation.retry_after_secs = self.session.retry_after_secs();
        scan.progress
            .pages
            .iter()
            .flat_map(|page| page.items.iter().map(Row::artist))
            .collect()
    }
}
