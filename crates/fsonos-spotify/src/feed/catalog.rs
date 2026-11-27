//! Bind a planned append to the account and catalog that supplied its music.
//! Rechecking this small receipt never reads the library or opens another store.

use fsonos_core::store::{SpotifyLibraryOwner, Store, StoreError};

use super::FeedError;

/// The accepted account and catalog revision used to build a selection pool.
/// Callers retain it beside that pool and pass it through [`super::Planning`].
/// It is an observation, not a transaction spanning speaker network requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogReceipt {
    owner: Option<SpotifyLibraryOwner>,
    revision: Option<u64>,
}

impl CatalogReceipt {
    pub fn read<S: Store + ?Sized>(store: &S) -> Result<Self, StoreError> {
        Ok(Self {
            owner: store.spotify_library_owner()?,
            revision: store.library_catalog_revision()?,
        })
    }

    /// Legacy store implementations without revisions cannot reuse a warmed
    /// pool, but their account identity can still be checked before an append.
    #[must_use]
    pub fn is_versioned(&self) -> bool {
        self.revision.is_some()
    }

    /// Run after fallible speaker preflight, immediately before sending an
    /// append. A stale or unreadable receipt rejects admission before any send;
    /// it is neither a speaker refusal nor an uncertain enqueue outcome.
    pub fn confirm<S: Store + ?Sized>(&self, store: &S) -> Result<(), FeedError> {
        if *self != Self::read(store)? {
            return Err(FeedError::CatalogChanged);
        }
        Ok(())
    }
}
