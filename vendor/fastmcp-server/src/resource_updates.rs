//! Application-owned resource invalidations for both supported protocol eras.
//!
//! The publisher is weak: it cannot keep a server, connection, request, or
//! notification transport alive. Modern delivery uses the existing final
//! subscription registry. Legacy entries refer to connection-owned subscription
//! state and are revoked when that exact connection generation closes.

use std::collections::{BTreeMap, BTreeSet};
#[cfg(any(feature = "legacy-2024-11-05", test))]
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

#[cfg(any(feature = "legacy-2024-11-05", test))]
use fastmcp_core::NotificationSender as _;
use fastmcp_core::{McpError, McpResult};
use fastmcp_protocol::common_types::AbsoluteUri;
use fastmcp_protocol::{FinalResourceUpdatedNotificationParams, ServerNotification};

use crate::{FinalSubscriptionPhase, FinalSubscriptionRegistry};
#[cfg(any(feature = "legacy-2024-11-05", test))]
use crate::{NotificationSender, handler::LogNotificationSender};
#[cfg(any(feature = "legacy-2024-11-05", test))]
use fastmcp_protocol::protocol_policy::ProtocolEra;

/// Maximum resource URIs accepted by one publication.
pub const MAX_RESOURCE_UPDATE_BATCH: usize = 64;
/// Maximum combined URI bytes accepted by one publication.
pub const MAX_RESOURCE_UPDATE_BATCH_BYTES: usize = 64 * 1024;

/// The currently subscribed resource identities, with duplicates removed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ResourceSubscriptions {
    /// Exact subscribed URIs, in lexical order. Keep their spelling when publishing.
    pub uris: Vec<String>,
    /// Live connections or final listen streams with resource subscriptions.
    pub subscribers: usize,
}

/// A resource-only publisher that does not retain its server's lifetime.
///
/// Obtain this before moving the server into an HTTP or stdio lifecycle. It is
/// safe to keep the handle on an application-owned event worker: closing the
/// server revokes delivery, including when an in-process listen handle remains.
#[derive(Clone)]
pub struct ResourceUpdates {
    registry: Weak<Registry>,
}

/// The server alone owns this guard. A publication may temporarily retain the
/// registry, but cannot defer this guard's revocation when the server drops.
pub(super) struct Owner {
    registry: Arc<Registry>,
}

impl Owner {
    pub(super) fn new(modern: &Arc<FinalSubscriptionRegistry>) -> Self {
        Self {
            registry: Arc::new(Registry::new(modern)),
        }
    }

    pub(super) fn publisher(&self) -> ResourceUpdates {
        ResourceUpdates {
            registry: Arc::downgrade(&self.registry),
        }
    }
}

impl std::ops::Deref for Owner {
    type Target = Registry;

    fn deref(&self) -> &Self::Target {
        &self.registry
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.registry.closed.store(true, Ordering::Release);
    }
}

pub(super) struct Registry {
    modern: Weak<FinalSubscriptionRegistry>,
    closed: AtomicBool,
    #[cfg(any(feature = "legacy-2024-11-05", test))]
    legacy: Mutex<Vec<Weak<LegacySubscriber>>>,
}

impl Registry {
    pub(super) fn new(modern: &Arc<FinalSubscriptionRegistry>) -> Self {
        Self {
            modern: Arc::downgrade(modern),
            closed: AtomicBool::new(false),
            #[cfg(any(feature = "legacy-2024-11-05", test))]
            legacy: Mutex::new(Vec::new()),
        }
    }

    fn open(&self) -> bool {
        !self.closed.load(Ordering::Acquire)
            && self
                .modern
                .upgrade()
                .is_some_and(|modern| !modern.terminating.load(Ordering::Acquire))
    }

    #[cfg(any(feature = "legacy-2024-11-05", test))]
    pub(super) fn register(
        &self,
        subscriptions: Arc<Mutex<Vec<String>>>,
        sender: NotificationSender,
    ) -> Arc<LegacySubscriber> {
        let subscriber = Arc::new(LegacySubscriber {
            subscriptions,
            sender,
            closed: AtomicBool::new(false),
        });
        let mut legacy = self
            .legacy
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        legacy.retain(|entry| entry.upgrade().is_some_and(|entry| !entry.is_closed()));
        legacy.push(Arc::downgrade(&subscriber));
        subscriber
    }

    #[cfg(any(feature = "legacy-2024-11-05", test))]
    fn legacy(&self) -> Vec<Arc<LegacySubscriber>> {
        let mut legacy = self
            .legacy
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut active = Vec::new();
        legacy.retain(|entry| {
            if let Some(entry) = entry.upgrade()
                && !entry.is_closed()
            {
                active.push(entry);
                true
            } else {
                false
            }
        });
        active
    }
}

impl ResourceUpdates {
    /// Whether the server has closed or begun shutting down.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.registry
            .upgrade()
            .is_none_or(|registry| !registry.open())
    }

    /// Snapshot the exact resource URIs admitted by ready live subscriptions.
    ///
    /// This only reads retained subscription metadata. It does not read a
    /// resource, contact the application, or invoke a transport callback.
    /// Modern listens become visible after their acknowledgement and opening
    /// event queue have finished, so an initial refresh cannot fill that queue.
    #[must_use]
    pub fn subscriptions(&self) -> ResourceSubscriptions {
        let Some(registry) = self.registry.upgrade().filter(|registry| registry.open()) else {
            return ResourceSubscriptions::default();
        };
        let mut uris = BTreeSet::new();
        let mut subscribers = 0;
        if let Some(modern) = registry.modern.upgrade() {
            let entries = modern
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entries
                .values()
                .cloned()
                .collect::<Vec<_>>();
            // Election paths can acquire the registry after the phase lock.
            // Release the registry lock before observing individual phases.
            for entry in entries {
                let ready = matches!(
                    *entry
                        .election
                        .phase
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                    FinalSubscriptionPhase::Active | FinalSubscriptionPhase::EventDelivery(_)
                );
                if !ready || entry.request_cancellation.is_cancel_requested() {
                    continue;
                }
                if let Some(selected) = &entry.accepted_filter.resource_subscriptions
                    && !selected.is_empty()
                {
                    subscribers += 1;
                    uris.extend(selected.iter().cloned());
                }
            }
        }
        #[cfg(any(feature = "legacy-2024-11-05", test))]
        for entry in registry.legacy() {
            let selected = entry
                .subscriptions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !entry.is_closed() && !selected.is_empty() {
                subscribers += 1;
                uris.extend(selected.iter().cloned());
            }
        }
        ResourceSubscriptions {
            uris: uris.into_iter().collect(),
            subscribers,
        }
    }

    /// Offer resource updates to matching live subscribers.
    ///
    /// At most 64 URIs and 64 KiB of URI text are accepted. The whole batch is
    /// validated before any delivery; duplicate URIs publish once. The returned
    /// count is notifications offered to matching transports, not a peer receipt.
    /// Modern filters, subscription IDs, acknowledgement ordering and cancellation
    /// remain the final registry's responsibility. Legacy delivery consults the
    /// current connection's subscription set, including unsubscriptions.
    ///
    /// # Errors
    ///
    /// Returns invalid parameters for an oversized batch or a malformed absolute
    /// URI, or an encoding error from the final subscription registry.
    pub fn publish(&self, uris: &[String]) -> McpResult<usize> {
        if uris.len() > MAX_RESOURCE_UPDATE_BATCH {
            return Err(McpError::invalid_params(
                "too many resource updates in one batch",
            ));
        }
        let mut bytes = 0_usize;
        let mut prepared = BTreeMap::new();
        for uri in uris {
            bytes = bytes.checked_add(uri.len()).ok_or_else(|| {
                McpError::invalid_params("resource update batch exceeds its byte budget")
            })?;
            if bytes > MAX_RESOURCE_UPDATE_BATCH_BYTES {
                return Err(McpError::invalid_params(
                    "resource update batch exceeds its byte budget",
                ));
            }
            let absolute = AbsoluteUri::parse(uri.as_str()).map_err(|_| {
                McpError::invalid_params("resource update URI must be an absolute URI")
            })?;
            prepared.entry(uri.clone()).or_insert(absolute);
        }
        let Some(registry) = self.registry.upgrade().filter(|registry| registry.open()) else {
            return Ok(0);
        };
        let mut count = 0;
        for (uri, absolute) in prepared {
            if !registry.open() {
                break;
            }
            if let Some(modern) = registry.modern.upgrade() {
                count += modern.publish(ServerNotification::ResourceUpdated(
                    FinalResourceUpdatedNotificationParams {
                        uri: absolute,
                        meta: None,
                        additional: BTreeMap::new(),
                    },
                ))?;
            }
            #[cfg(any(feature = "legacy-2024-11-05", test))]
            for entry in registry.legacy() {
                if registry.open() {
                    count += usize::from(entry.publish(&uri));
                }
            }
            #[cfg(not(any(feature = "legacy-2024-11-05", test)))]
            let _ = uri;
        }
        Ok(count)
    }
}

/// Only the live connection runtime owns this entry. The publisher registry
/// retains a weak reference and cannot extend a disconnected transport's life.
#[cfg(any(feature = "legacy-2024-11-05", test))]
pub(super) struct LegacySubscriber {
    subscriptions: Arc<Mutex<Vec<String>>>,
    sender: NotificationSender,
    closed: AtomicBool,
}

#[cfg(any(feature = "legacy-2024-11-05", test))]
impl LegacySubscriber {
    pub(super) fn close(&self) {
        // No mutex or callback is awaited on the connection's shutdown path.
        self.closed.store(true, Ordering::Release);
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    fn publish(&self, uri: &str) -> bool {
        let subscribed = {
            let selected = self
                .subscriptions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            selected.iter().any(|selected| selected == uri)
        };
        if !subscribed || self.is_closed() {
            return false;
        }
        // Membership admits this publication. Unsubscribe fences subsequent
        // admissions; an already-started invalidation may still finish. Never
        // hold subscription metadata across an embedding transport callback.
        let sender = LogNotificationSender::new(
            |notification| (self.sender)(notification),
            ProtocolEra::Legacy2024,
        );
        sender.send_resource_updated(uri);
        true
    }
}

#[cfg(test)]
mod tests;
