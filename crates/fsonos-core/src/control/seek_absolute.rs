//! Observe a seek's effect against its original source and queue occurrence.
//! Absolute and relative seeks share this bounded recovery proof. Preparation
//! is optional; the native request is still sent once when a player cannot
//! provide enough evidence to recover a lost reply.

use super::{SourceDestination, seek_relative::reached};
use crate::CoreError;
use fsonos_proto::content::{self, BrowseFlag, BrowsePage, QUEUE};
use fsonos_proto::control::{self as soap, PositionInfo};
use fsonos_proto::didl::{DidlKind, spotify_uri_from_renderer_uri};
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::PlayerId;
use std::net::IpAddr;
use std::time::{Duration, Instant};

const PREPARE_BUDGET: Duration = Duration::from_millis(750);
const CONFIRM_BUDGET: Duration = Duration::from_secs(3);
const MAX_OBSERVATION_AGE: Duration = Duration::from_secs(30);

struct Reads<'a, T: ?Sized> {
    inner: &'a T,
    until: Instant,
}

impl<T: Transport + ?Sized> Transport for Reads<'_, T> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if Instant::now() >= self.until {
            return Err(ProtoError::NotWired("seek observation budget elapsed"));
        }
        self.inner.soap_post(host, path, action, body)
    }
}

pub(super) fn seek<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    coordinator: &PlayerId,
    target: u32,
) -> Result<u32, CoreError> {
    let preparation = Reads {
        inner: t,
        until: Instant::now() + PREPARE_BUDGET,
    };
    let proof = Observation::read(&preparation, host, coordinator)
        .ok()
        .flatten()
        .filter(|_| Instant::now() < preparation.until);
    super::require_coordinator(t, host, coordinator)?;
    let sent = Instant::now();
    let Err(error) = soap::seek_position(t, host, target) else {
        return Ok(target);
    };
    if matches!(
        &error,
        ProtoError::Network { .. } | ProtoError::Malformed(_)
    ) && proof.is_some_and(|proof| proof.recovered(t, host, coordinator, target, sent))
    {
        tracing::warn!(cause = %error, "absolute seek confirmed without repeating its write");
        Ok(target)
    } else {
        // A refusal or unavailable read-back must retain the original error.
        Err(error.into())
    }
}

pub(super) struct Observation {
    position: PositionInfo,
    destination: SourceDestination,
    queue: Option<BrowsePage>,
    observed: Instant,
}

impl Observation {
    fn read<T: Transport + ?Sized>(
        t: &T,
        host: IpAddr,
        coordinator: &PlayerId,
    ) -> Result<Option<Self>, ProtoError> {
        let observed = Instant::now();
        let position = soap::get_position_info(t, host)?;
        Self::from_position(t, host, coordinator, position, observed)
    }

    /// A relative seek already needs a real clock to compute its target.
    /// Retain that exact sample, rather than rereading and applying the offset
    /// to a different track. Missing optional evidence still allows one write.
    pub(super) fn for_relative<T: Transport + ?Sized>(
        t: &T,
        host: IpAddr,
        coordinator: &PlayerId,
        position: PositionInfo,
        observed: Instant,
    ) -> Option<Self> {
        let preparation = Reads {
            inner: t,
            until: observed + PREPARE_BUDGET,
        };
        Self::from_position(&preparation, host, coordinator, position, observed)
            .ok()
            .flatten()
            .filter(|_| Instant::now() < preparation.until)
    }

    fn from_position<T: Transport + ?Sized>(
        t: &T,
        host: IpAddr,
        coordinator: &PlayerId,
        position: PositionInfo,
        observed: Instant,
    ) -> Result<Option<Self>, ProtoError> {
        if position.uri.trim().is_empty() || position.uri == "NOT_IMPLEMENTED" {
            return Ok(None);
        }
        let destination = SourceDestination::read(t, host, coordinator)?;
        if !destination.has_boot_identity() {
            return Ok(None);
        }
        let queue = if destination.is_queue(coordinator) {
            let Some(offset) = position.track.checked_sub(1) else {
                return Ok(None);
            };
            let page = content::browse(t, host, QUEUE, BrowseFlag::DirectChildren, offset, 1)?;
            if page.update_id == 0
                || page.number_returned != 1
                || page.objects.len() != 1
                || page.total_matches < position.track
                || page.total_matches > content::MAX_BROWSE_OBJECTS
            {
                return Ok(None);
            }
            let item = &page.objects[0];
            if item.kind != DidlKind::Item
                || item.res.as_ref().is_none_or(|resource| {
                    resource.uri.trim().is_empty()
                        || resource.uri == "NOT_IMPLEMENTED"
                        || !same_track(&position.uri, &resource.uri)
                })
            {
                return Ok(None);
            }
            Some(page)
        } else {
            None
        };
        Ok(Some(Self {
            position,
            destination,
            queue,
            observed,
        }))
    }

    pub(super) fn recovered<T: Transport + ?Sized>(
        &self,
        t: &T,
        host: IpAddr,
        coordinator: &PlayerId,
        target: u32,
        sent: Instant,
    ) -> bool {
        let confirmation = Reads {
            inner: t,
            until: Instant::now() + CONFIRM_BUDGET,
        };
        self.confirmed(&confirmation, host, coordinator, target, sent)
            .unwrap_or(false)
            && Instant::now() < confirmation.until
    }

    fn confirmed<T: Transport + ?Sized>(
        &self,
        t: &T,
        host: IpAddr,
        coordinator: &PlayerId,
        target: u32,
        sent: Instant,
    ) -> Result<bool, ProtoError> {
        if self.observed.elapsed() > MAX_OBSERVATION_AGE {
            return Ok(false);
        }
        let first = soap::get_position_info(t, host)?;
        let transport = soap::get_transport_info(t, host)?;
        if transport.status == "ERROR_OCCURRED"
            || !reached(
                &self.position,
                &first,
                target,
                transport.state,
                sent.elapsed(),
            )
        {
            return Ok(false);
        }
        self.destination.confirm(t, host, coordinator)?;
        if let Some(queue) = &self.queue {
            let current = content::browse(
                t,
                host,
                QUEUE,
                BrowseFlag::DirectChildren,
                self.position.track - 1,
                1,
            )?;
            if *queue != current {
                return Ok(false);
            }
        }
        let last = soap::get_position_info(t, host)?;
        Ok(self.observed.elapsed() <= MAX_OBSERVATION_AGE
            && reached(
                &self.position,
                &last,
                target,
                transport.state,
                sent.elapsed(),
            ))
    }
}

fn same_track(left: &str, right: &str) -> bool {
    left == right
        || spotify_uri_from_renderer_uri(left)
            .is_some_and(|uri| Some(uri) == spotify_uri_from_renderer_uri(right))
}
