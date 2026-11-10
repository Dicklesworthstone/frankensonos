//! Keep cached selection music attached to the accepted account and catalog.
//! Library publication, account adoption and optional enrichment are separate
//! transactions; a refresh can fail after changing what may be selected.

use super::{Failure, PLAN_RETRY, State, WorkPool};
use fsonos_api::ErrorCode;
use fsonos_core::store::{SpotifyLibraryOwner, Store};
use std::time::Instant;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Receipt {
    owner: Option<SpotifyLibraryOwner>,
    revision: Option<u64>,
}

impl Receipt {
    pub(super) fn read(store: &dyn Store) -> Result<Self, Failure> {
        let read = || {
            Ok(Self {
                owner: store.spotify_library_owner()?,
                revision: store.library_catalog_revision()?,
            })
        };
        read().map_err(|error: fsonos_core::store::StoreError| {
            Failure::new(
                ErrorCode::Internal,
                format!("the DJ's library identity could not be read: {error}"),
            )
        })
    }

    pub(super) fn confirm(&self, store: &dyn Store) -> Result<(), Failure> {
        if *self != Self::read(store)? {
            return Err(Failure::new(
                ErrorCode::NotReady,
                "the Spotify account or library changed while the DJ's selection pool was being read",
            ));
        }
        Ok(())
    }

    fn permits_reuse(&self, current: &Self) -> bool {
        self.revision.is_some() && self == current
    }
}

impl State {
    /// Ordinary listening/feedback writes do not change this receipt. When
    /// catalog evidence changes or cannot be read, a failed rebuild must never
    /// fall back to the previous account's or retired music. Existing queued
    /// works live in the feeds and are left alone.
    pub(super) fn check_catalog(&mut self, store: &dyn Store) -> Result<(), Failure> {
        let Some(previous) = self.catalog.as_ref() else {
            return Ok(());
        };
        let current = Receipt::read(store);
        if current
            .as_ref()
            .is_ok_and(|current| previous.permits_reuse(current))
        {
            return Ok(());
        }
        self.pool = WorkPool::default();
        self.feedback = None;
        self.catalog = None;
        self.planned = false;
        self.plan_retry_at = current.is_err().then(|| Instant::now() + PLAN_RETRY);
        current.map(|_| ())
    }
}

#[cfg(test)]
mod tests;
