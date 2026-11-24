//! Token refresh and new sign-in have different library consequences.
//! The cache's opaque receipt distinguishes them without retaining credentials
//! in scheduler state or making an account request for each selection.

use std::path::Path;

use fsonos_api::{ErrorCode, Failure};
use fsonos_spotify::client::TokenCache;

use super::{LibrarySync, continuation};

impl LibrarySync {
    pub(super) fn authorization_id(&self) -> Result<Option<String>, Failure> {
        current(&self.data_dir)
    }
}

fn current(data_dir: &Path) -> Result<Option<String>, Failure> {
    TokenCache::in_data_dir(data_dir)
        .authorization_id()
        .map_err(super::failure)
}

pub(in crate::dj) fn pending() -> Failure {
    Failure::new(
        ErrorCode::NotReady,
        "Spotify sign-in changed; new DJ selections wait for this sign-in's full library refresh",
    )
    .with_hint(
        "Check fsonos dj sync --status; a bounded refresh resumes automatically, \
         or request fsonos dj sync.",
    )
}

/// Only a completed full refresh can acknowledge a sign-in for selection.
/// With no credentials ever admitted (e.g. a local test library), retain the
/// existing cache-only behavior.
pub(in crate::dj) fn selection(data_dir: &Path) -> Result<Option<String>, Failure> {
    let current = current(data_dir)?;
    let saved = continuation::load_checked(data_dir)?.unwrap_or_default();
    match current {
        Some(receipt) if saved.accepted_authorization_id.as_ref() == Some(&receipt) => {
            Ok(Some(receipt))
        }
        None if !saved.authorization_observed
            && saved.authorization_id.is_none()
            && saved.accepted_authorization_id.is_none() =>
        {
            Ok(None)
        }
        _ => Err(pending()),
    }
}
