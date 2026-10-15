//! State-setting playback commands can confirm a lost reply without another
//! write. Definite refusals keep their existing meaning and recovery path.

use super::source_start;
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::{PlayerId, TransportState};
use std::net::IpAddr;

/// Resume is a state request, unlike Next/Previous. Reuse the source-start
/// transition wait, proving coordinator ownership around each observation.
pub(super) fn play<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    coordinator: &PlayerId,
) -> Result<(), ProtoError> {
    complete("Play", soap::play(t, host), || {
        source_start::wait_for_playing(t, host, || {
            Ok(get_zone_group_state(t, host)?
                .groups
                .iter()
                .any(|group| group.coordinator == *coordinator))
        })
    })
}

pub(super) fn stop<T: Transport + ?Sized>(t: &T, host: IpAddr) -> Result<(), ProtoError> {
    complete("Stop", soap::stop(t, host), || {
        Ok(soap::get_transport_info(t, host)?.state == TransportState::Stopped)
    })
}

/// Stop an unpausable stream only after a definite 701 and an active state.
/// A lost Pause reply is not a refusal and must never trigger another write.
pub(super) fn pause<T: Transport + ?Sized>(t: &T, host: IpAddr) -> Result<(), ProtoError> {
    let refused = match soap::pause(t, host) {
        Err(error @ ProtoError::SoapFault { code: 701, .. }) => error,
        result => {
            return complete("Pause", result, || {
                Ok(matches!(
                    soap::get_transport_info(t, host)?.state,
                    TransportState::Paused | TransportState::Stopped
                ))
            });
        }
    };
    match soap::get_transport_info(t, host).map(|info| info.state) {
        Ok(TransportState::Paused | TransportState::Stopped) => Ok(()),
        Ok(TransportState::Playing | TransportState::Transitioning) => stop(t, host),
        _ => Err(refused),
    }
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
