//! Keep cached selection music attached to the accepted account and catalog.
//! Library publication, account adoption and optional enrichment are separate
//! transactions; a refresh can fail after changing what may be selected.

use super::{Failure, PLAN_RETRY, State, WorkPool};
use fsonos_api::ErrorCode;
use fsonos_core::store::Store;
use fsonos_spotify::feed::CatalogReceipt;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Receipt {
    catalog: CatalogReceipt,
    data_dir: Option<PathBuf>,
    authorization_id: Option<String>,
}

impl Receipt {
    pub(super) fn read(store: &dyn Store, data_dir: Option<&Path>) -> Result<Self, Failure> {
        let authorization_id = data_dir
            .map(super::sync::authorization::selection)
            .transpose()?
            .flatten();
        let catalog = CatalogReceipt::read(store).map_err(|error| {
            Failure::new(
                ErrorCode::Internal,
                format!("the DJ's library identity could not be read: {error}"),
            )
        })?;
        Ok(Self {
            catalog,
            data_dir: data_dir.map(Path::to_owned),
            authorization_id,
        })
    }

    pub(super) fn confirm(&self, store: &dyn Store) -> Result<(), Failure> {
        if *self != Self::read(store, self.data_dir.as_deref())? {
            return Err(Failure::new(
                ErrorCode::NotReady,
                "the Spotify account or library changed while the DJ's selection pool was being read",
            ));
        }
        Ok(())
    }

    fn confirm_authorization(&self) -> Result<(), Failure> {
        if let Some(data_dir) = &self.data_dir
            && super::sync::authorization::selection(data_dir)? != self.authorization_id
        {
            return Err(super::sync::authorization::pending());
        }
        Ok(())
    }

    fn permits_reuse(&self, current: &Self) -> bool {
        self.catalog.is_versioned() && self == current
    }

    pub(super) fn catalog(&self) -> &CatalogReceipt {
        &self.catalog
    }
}

impl State {
    /// Read-only readiness may borrow the live pool only while both its
    /// catalog proof and configuration still match. It must not retire or
    /// publish a live plan, consume a sync notice, or alter a feed's RNG.
    pub(super) fn plan_is_current(
        &self,
        store: &dyn Store,
        moods_file: Option<&std::path::PathBuf>,
    ) -> Result<bool, Failure> {
        if !self.planned || self.plan_inputs != Some(super::plan_inputs(moods_file)) {
            return Ok(false);
        }
        let Some(previous) = &self.catalog else {
            return Ok(false);
        };
        Ok(previous.permits_reuse(&Receipt::read(
            store,
            moods_file.and_then(|path| path.parent()),
        )?))
    }

    /// Ordinary listening/feedback writes do not change this receipt. When
    /// catalog evidence changes or cannot be read, a failed rebuild must never
    /// fall back to the previous account's or retired music. Existing queued
    /// works live in the feeds and are left alone.
    pub(super) fn check_catalog(&mut self, store: &dyn Store) -> Result<(), Failure> {
        let Some(previous) = self.catalog.as_ref() else {
            return Ok(());
        };
        let current = Receipt::read(store, previous.data_dir.as_deref());
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

/// A speaker preflight can take long enough for another process to complete
/// sign-in. Recheck immediately before appending any selected music. Reads,
/// stopping, and playback of an already-owned queue retain their usual path.
pub(super) struct SelectionTransport<'a> {
    inner: &'a dyn fsonos_proto::Transport,
    receipt: Option<Receipt>,
}

impl<'a> SelectionTransport<'a> {
    pub(super) fn new(inner: &'a dyn fsonos_proto::Transport, receipt: Option<&Receipt>) -> Self {
        Self {
            inner,
            receipt: receipt.cloned(),
        }
    }
}

impl fsonos_proto::Transport for SelectionTransport<'_> {
    fn soap_post(
        &self,
        host: std::net::IpAddr,
        control_path: &str,
        soap_action: &str,
        body: &str,
    ) -> Result<String, fsonos_proto::ProtoError> {
        let action = soap_action.trim_matches('"').rsplit('#').next();
        if matches!(action, Some("AddURIToQueue" | "AddMultipleURIsToQueue")) {
            self.receipt
                .as_ref()
                .ok_or_else(super::sync::authorization::pending)
                .and_then(Receipt::confirm_authorization)
                .map_err(|error| fsonos_proto::ProtoError::Network {
                    target: "the DJ's selection authorization".into(),
                    detail: error.detail,
                })?;
        }
        self.inner.soap_post(host, control_path, soap_action, body)
    }

    fn http_get(&self, url: &str) -> Result<String, fsonos_proto::ProtoError> {
        self.inner.http_get(url)
    }

    fn http_get_bytes(
        &self,
        url: &str,
    ) -> Result<fsonos_proto::HttpBody, fsonos_proto::ProtoError> {
        self.inner.http_get_bytes(url)
    }
}

#[cfg(test)]
mod tests;
