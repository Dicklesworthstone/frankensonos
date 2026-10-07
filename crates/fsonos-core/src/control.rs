//! Control orchestration: carry out a command addressed to a player.
//!
//! The surfaces decide which player a request addresses (fsonos-api plans a
//! request into a coordinator- or room-addressed command); this module finds
//! that player on the LAN and sends the SOAP actions. Every mutating command
//! from every surface comes through here, so this is where house policy and
//! the action log hook in.

use crate::{CoreError, HouseholdState};
use fsonos_proto::Transport;
use fsonos_proto::control::{self as soap, PositionInfo, TransportInfo};
use fsonos_types::{Player, PlayerId};
use std::net::IpAddr;

/// The player `id`, in whichever household knows it.
pub fn locate<'a>(
    households: &'a [HouseholdState],
    id: &PlayerId,
) -> Result<&'a Player, CoreError> {
    households
        .iter()
        .find_map(|h| h.player(id))
        .ok_or_else(|| CoreError::UnknownPlayer(id.0.clone()))
}

fn addr(households: &[HouseholdState], id: &PlayerId) -> Result<IpAddr, CoreError> {
    locate(households, id).map(|p| p.ip)
}

/// Resume (or start) playback on the group `coordinator` leads.
pub fn resume<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
) -> Result<(), CoreError> {
    Ok(soap::play(t, addr(households, coordinator)?)?)
}

pub fn pause<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
) -> Result<(), CoreError> {
    Ok(soap::pause(t, addr(households, coordinator)?)?)
}

pub fn stop<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
) -> Result<(), CoreError> {
    Ok(soap::stop(t, addr(households, coordinator)?)?)
}

pub fn next<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
) -> Result<(), CoreError> {
    Ok(soap::next(t, addr(households, coordinator)?)?)
}

pub fn previous<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
) -> Result<(), CoreError> {
    Ok(soap::previous(t, addr(households, coordinator)?)?)
}

/// Point the group `coordinator` leads at a renderer-ready `uri` (with its
/// DIDL-Lite `metadata`, possibly empty) and start it.
pub fn play_uri<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    uri: &str,
    metadata: &str,
) -> Result<(), CoreError> {
    let host = addr(households, coordinator)?;
    soap::set_av_transport_uri(t, host, uri, metadata)?;
    Ok(soap::play(t, host)?)
}

/// Set one room's volume (on the room's own player); returns the new level.
pub fn set_volume<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    player: &PlayerId,
    level: u8,
) -> Result<u8, CoreError> {
    let level = level.min(100);
    soap::set_volume(t, addr(households, player)?, level)?;
    Ok(level)
}

/// Raise or lower one room's volume; returns the new level.
pub fn adjust_volume<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    player: &PlayerId,
    delta: i32,
) -> Result<u8, CoreError> {
    Ok(soap::set_relative_volume(
        t,
        addr(households, player)?,
        delta,
    )?)
}

/// Set the group volume of the group `coordinator` leads; members keep their
/// balance. Returns the new level.
pub fn set_group_volume<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    level: u8,
) -> Result<u8, CoreError> {
    let level = level.min(100);
    soap::set_group_volume(t, addr(households, coordinator)?, level)?;
    Ok(level)
}

/// Raise or lower the group volume; returns the new level.
pub fn adjust_group_volume<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    delta: i32,
) -> Result<u8, CoreError> {
    Ok(soap::set_relative_group_volume(
        t,
        addr(households, coordinator)?,
        delta,
    )?)
}

pub fn set_mute<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    player: &PlayerId,
    mute: bool,
) -> Result<(), CoreError> {
    Ok(soap::set_mute(t, addr(households, player)?, mute)?)
}

/// Move `member` into the group `coordinator` leads.
pub fn join<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    member: &PlayerId,
    coordinator: &PlayerId,
) -> Result<(), CoreError> {
    // Both must be known: a join across households can never work.
    locate(households, coordinator)?;
    Ok(soap::join_group(t, addr(households, member)?, coordinator)?)
}

/// Take `member` out of its group into a group of its own.
pub fn leave<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    member: &PlayerId,
) -> Result<(), CoreError> {
    Ok(soap::leave_group(t, addr(households, member)?)?)
}

/// What the group `coordinator` leads is doing right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Playback {
    pub transport: TransportInfo,
    pub position: PositionInfo,
}

pub fn playback<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
) -> Result<Playback, CoreError> {
    let host = addr(households, coordinator)?;
    Ok(Playback {
        transport: soap::get_transport_info(t, host)?,
        position: soap::get_position_info(t, host)?,
    })
}

/// One room's volume, read from its own player.
pub fn volume<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    player: &PlayerId,
) -> Result<u8, CoreError> {
    Ok(soap::get_volume(t, addr(households, player)?)?)
}
