//! State-setting playback commands can confirm a lost reply without another
//! write. Definite refusals keep their existing meaning and recovery path.

use super::source_start;
use crate::CoreError;
use crate::topology::read_at;
use fsonos_proto::control as soap;
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::{PlayerId, TransportState};
use std::net::IpAddr;

/// Resume is a state request, unlike Next/Previous. Reuse the source-start
/// transition wait, proving coordinator ownership around each observation.
pub(super) fn play<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    coordinator: &PlayerId,
) -> Result<(), CoreError> {
    super::require_coordinator(t, host, coordinator)?;
    Ok(complete("Play", soap::play(t, host), || {
        source_start::wait_for_playing(t, host, || {
            crate::topology::coordinator_at(&read_at(t, host, coordinator)?, host, coordinator)
        })
    })?)
}

pub(super) fn stop<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    coordinator: Option<&PlayerId>,
) -> Result<(), ProtoError> {
    complete("Stop", soap::stop(t, host), || {
        if !same_coordinator(t, host, coordinator)? {
            return Ok(false);
        }
        let stopped = soap::get_transport_info(t, host)?.state == TransportState::Stopped;
        Ok(stopped && same_coordinator(t, host, coordinator)?)
    })
}

/// Stop an unpausable stream only after a definite 701 and an active state.
/// A lost Pause reply is not a refusal and must never trigger another write.
/// Named calls retain their coordinator through read-back and fallback Stop;
/// an anonymous pause has no identity claim to validate.
pub(super) fn pause<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    coordinator: Option<&PlayerId>,
) -> Result<(), ProtoError> {
    let refused = match soap::pause(t, host) {
        Err(error @ ProtoError::SoapFault { code: 701, .. }) => error,
        result => {
            return complete("Pause", result, || {
                if !same_coordinator(t, host, coordinator)? {
                    return Ok(false);
                }
                let inactive = matches!(
                    soap::get_transport_info(t, host)?.state,
                    TransportState::Paused | TransportState::Stopped
                );
                Ok(inactive && same_coordinator(t, host, coordinator)?)
            });
        }
    };
    if !same_coordinator(t, host, coordinator).unwrap_or(false) {
        return Err(refused);
    }
    let state = match soap::get_transport_info(t, host) {
        Ok(info) => info.state,
        Err(_) => return Err(refused),
    };
    if !same_coordinator(t, host, coordinator).unwrap_or(false) {
        return Err(refused);
    }
    match state {
        TransportState::Paused | TransportState::Stopped => Ok(()),
        TransportState::Playing | TransportState::Transitioning => stop(t, host, coordinator),
        _ => Err(refused),
    }
}

fn same_coordinator<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    coordinator: Option<&PlayerId>,
) -> Result<bool, ProtoError> {
    let Some(coordinator) = coordinator else {
        return Ok(true);
    };
    crate::topology::coordinator_at(&read_at(t, host, coordinator)?, host, coordinator)
}

fn complete(
    operation: &'static str,
    result: Result<(), ProtoError>,
    observe: impl FnOnce() -> Result<bool, ProtoError>,
) -> Result<(), ProtoError> {
    let Err(error) = result else {
        return Ok(());
    };
    let ambiguous = matches!(
        &error,
        ProtoError::Network { .. } | ProtoError::Malformed(_)
    );
    if ambiguous && observe().unwrap_or(false) {
        tracing::warn!(
            operation,
            cause = %error,
            "playback state confirmed without another write"
        );
        Ok(())
    } else {
        // In particular, a failed read must not replace the original fault,
        // and an explicit UPnP refusal is never inferred to have succeeded.
        Err(error)
    }
}

#[cfg(test)]
mod tests;
