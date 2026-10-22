//! Carry a queue's ordering with a copied source, without replaying settings
//! whose replies are lost. All checks stay on the prepared destination queue.

use super::{MoveError, invalid};
use fsonos_proto::content::{self, BrowseFlag, QUEUE};
use fsonos_proto::control as soap;
use fsonos_proto::play_mode::{get_play_mode, set_play_mode};
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::{PlayMode, PlayerId};
use std::net::IpAddr;

fn queue_unchanged<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    target: &PlayerId,
    revision: u32,
) -> Result<(), MoveError> {
    let uri = format!("x-rincon-queue:{}#0", target.0);
    if soap::get_media_info(t, host)?.uri != uri {
        return Err(invalid(
            "the destination no longer renders the copied queue",
        ));
    }
    let page = content::browse(t, host, QUEUE, BrowseFlag::DirectChildren, 0, 1)?;
    if page.update_id != revision || soap::get_media_info(t, host)?.uri != uri {
        return Err(invalid(
            "the destination queue changed while restoring its play mode",
        ));
    }
    Ok(())
}

pub(super) fn confirm<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    target: &PlayerId,
    revision: u32,
    expected: PlayMode,
) -> Result<(), MoveError> {
    if get_play_mode(t, host)? != expected {
        return Err(invalid(
            "the destination did not retain the source queue's play mode",
        ));
    }
    queue_unchanged(t, host, target, revision)
}

pub(super) fn apply<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    target: &PlayerId,
    revision: u32,
    expected: PlayMode,
) -> Result<(), MoveError> {
    queue_unchanged(t, host, target, revision)?;
    let current = get_play_mode(t, host)?;
    // Reading the setting can race a source switch or an edit to the queue.
    queue_unchanged(t, host, target, revision)?;
    if current == expected {
        return Ok(());
    }
    let result = set_play_mode(t, host, expected);
    if let Err(error) = &result
        && !matches!(error, ProtoError::Network { .. } | ProtoError::Malformed(_))
    {
        return result.map_err(MoveError::from);
    }
    // Read-back can confirm a lost reply, but never authorizes a second write.
    // A failed proof propagates before Seek/Play and before stopping the source.
    let proof = confirm(t, host, target, revision, expected);
    match (result, proof) {
        (_, Ok(())) => Ok(()),
        (Err(error), Err(_)) => Err(error.into()),
        (Ok(()), Err(error)) => Err(error),
    }
}
