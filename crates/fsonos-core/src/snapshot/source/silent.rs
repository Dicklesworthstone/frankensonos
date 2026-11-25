//! Keep inactive snapshots silent even on renderers that retain transport state
//! when selecting a source. Pausing is authorized separately from selecting it.

use super::{Anchor, Target, unproven};
use crate::snapshot::RestoreOp;
use crate::{CoreError, HouseholdState, control};
use fsonos_proto::control as soap;
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::{PlayerId, TransportState};
use std::net::IpAddr;

fn inactive(state: TransportState) -> bool {
    matches!(state, TransportState::Paused | TransportState::Stopped)
}

fn same_source<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    coordinator: &PlayerId,
    before: &Anchor,
) -> Result<Anchor, CoreError> {
    let current = Anchor::read(t, host, coordinator)?;
    if !before.same_source(&current)
        || !crate::snapshot::same_media(&before.media, &soap::get_media_info(t, host)?)
    {
        return Err(unproven(
            "source or group changed while preparing a silent restoration",
        ));
    }
    Ok(current)
}

/// Only the existing pause helper's Pause and fallback Stop cross this guard.
/// Its preparatory reads cannot authorize pausing another app's newer source.
struct Pause<'a, T: ?Sized> {
    inner: &'a T,
    host: IpAddr,
    coordinator: &'a PlayerId,
    before: &'a Anchor,
    vet: &'a dyn Fn(&RestoreOp) -> Result<RestoreOp, String>,
}

impl<T: Transport + ?Sized> Transport for Pause<'_, T> {
    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        self.inner.http_get(url)
    }

    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if action.ends_with("#Pause\"") || action.ends_with("#Stop\"") {
            if host != self.host
                || !(self.vet)(&RestoreOp::Stop).is_ok_and(|op| op == RestoreOp::Stop)
            {
                return Err(ProtoError::Malformed(
                    "silent restoration pause was not authorized".into(),
                ));
            }
            same_source(self.inner, host, self.coordinator, self.before).map_err(|error| {
                match error {
                    CoreError::Proto(error) => error,
                    error => ProtoError::Malformed(error.to_string()),
                }
            })?;
        }
        self.inner.soap_post(host, path, action, body)
    }
}

pub(super) fn prepare<T: Transport + ?Sized>(
    t: &T,
    household: &HouseholdState,
    host: IpAddr,
    coordinator: &PlayerId,
    before: Anchor,
    vet: &dyn Fn(&RestoreOp) -> Result<RestoreOp, String>,
) -> Result<Anchor, CoreError> {
    if inactive(before.state) {
        return Ok(before);
    }
    if before.state == TransportState::Unknown {
        return Err(unproven(
            "playback state is unknown before silent restoration",
        ));
    }
    let guarded = Pause {
        inner: t,
        host,
        coordinator,
        before: &before,
        vet,
    };
    control::pause_confirmed(&guarded, std::slice::from_ref(household), coordinator)?;
    let after = same_source(t, host, coordinator, &before)?;
    if !inactive(after.state) {
        return Err(unproven(
            "playback resumed while preparing a silent restoration",
        ));
    }
    Ok(after)
}

pub(super) fn before_selection<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    coordinator: &PlayerId,
    before: &Anchor,
) -> Result<(), CoreError> {
    let current = same_source(t, host, coordinator, before)?;
    if !inactive(current.state) || !inactive(soap::get_transport_info(t, host)?.state) {
        return Err(unproven(
            "playback is no longer inactive before source restoration",
        ));
    }
    Ok(())
}

pub(super) fn after_selection<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    coordinator: &PlayerId,
    target: &Target,
    members: &[PlayerId],
) -> Result<(), CoreError> {
    if !inactive(soap::get_transport_info(t, host)?.state)
        || !target.confirmed(t, host, coordinator, members)?
        || !inactive(soap::get_transport_info(t, host)?.state)
    {
        return Err(unproven(
            "restored source was not confirmed inactive; no seek or resume was sent",
        ));
    }
    Ok(())
}
