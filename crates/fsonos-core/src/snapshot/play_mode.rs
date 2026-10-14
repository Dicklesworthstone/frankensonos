//! Restore a queue's mode without restarting it or replaying uncertain writes.

use crate::CoreError;
use fsonos_proto::control as soap;
use fsonos_proto::play_mode::{get_play_mode, set_play_mode};
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::{PlayMode, PlayerId};
use std::net::IpAddr;

pub(super) fn restore<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    coordinator: &PlayerId,
    mode: PlayMode,
) -> Result<(), CoreError> {
    let queue = format!("x-rincon-queue:{}#0", coordinator.0);
    if soap::get_media_info(t, host)?.uri != queue {
        return Err(ProtoError::Malformed("play mode source is no longer its queue".into()).into());
    }
    let current = get_play_mode(t, host)?;
    if soap::get_media_info(t, host)?.uri != queue {
        return Err(
            ProtoError::Malformed("play mode source changed during its read".into()).into(),
        );
    }
    if current == mode {
        return Ok(());
    }
    let result = set_play_mode(t, host, mode);
    if let Err(error) = &result
        && !matches!(error, ProtoError::Network { .. } | ProtoError::Malformed(_))
    {
        return result.map_err(CoreError::from);
    }
    // Both accepted and ambiguous replies need read-back. Never repeat a mode
    // write: another controller may have changed it after the original request.
    let confirmed = get_play_mode(t, host)
        .and_then(|actual| Ok(actual == mode && soap::get_media_info(t, host)?.uri == queue));
    if confirmed.unwrap_or(false) {
        return Ok(());
    }
    match result {
        Err(error) => Err(error.into()),
        Ok(()) => Err(ProtoError::Malformed("queue play mode was not restored".into()).into()),
    }
}
