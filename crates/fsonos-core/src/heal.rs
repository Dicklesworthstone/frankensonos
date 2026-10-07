//! Self-healing calls: a command that fails because a player moved to a new
//! address, or because its group's coordinator changed under it, is retried
//! once after the model catches up.
//!
//! Players are addressed by UUID; their address is an attribute the survey
//! refreshes. Use these wrappers only for idempotent commands (transport,
//! volume, mute, URI, grouping): a retried `AddURIToQueue` could enqueue
//! twice, so queue appends are never retried blindly.

use crate::inventory::survey;
use crate::{CoreError, HouseholdState, control};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::PlayerId;
use std::net::IpAddr;
use std::time::Duration;

/// How long a re-resolving survey may wait for SSDP replies.
pub const RESOLVE_WAIT: Duration = Duration::from_secs(3);

/// What a self-healing call had to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recovery {
    /// It worked the first time.
    None,
    /// The player had moved; the call was retried at its new address.
    Readdressed {
        player: PlayerId,
        from: IpAddr,
        to: IpAddr,
    },
    /// The group's coordinator had changed; the call was retried on the new
    /// one.
    CoordinatorMoved { from: PlayerId, to: PlayerId },
}

/// Whether `e` means the player did not answer at all (as opposed to
/// answering with a fault).
#[must_use]
pub fn is_unreachable(e: &CoreError) -> bool {
    matches!(e, CoreError::Proto(ProtoError::Network { .. }))
}

/// Run `op` against `player`. If the player does not answer, survey again
/// (SSDP plus `seeds`), and if it now answers at a different address,
/// replace `households` with the new model and retry `op` once. Otherwise
/// the original error stands.
pub fn readdressing<T, R>(
    t: &T,
    seeds: &[IpAddr],
    households: &mut Vec<HouseholdState>,
    player: &PlayerId,
    op: impl Fn(&[HouseholdState]) -> Result<R, CoreError>,
) -> Result<(R, Recovery), CoreError>
where
    T: Transport + ?Sized,
{
    let first = op(households);
    let err = match first {
        Ok(r) => return Ok((r, Recovery::None)),
        Err(e) if is_unreachable(&e) => e,
        Err(e) => return Err(e),
    };
    let Ok(from) = control::locate(households, player).map(|p| p.ip) else {
        return Err(err);
    };
    let Ok(fresh) = survey(t, seeds, RESOLVE_WAIT) else {
        return Err(err);
    };
    let to = fresh
        .households
        .iter()
        .find_map(|h| h.player(player))
        .map(|p| p.ip);
    match to {
        Some(to) if to != from => {
            *households = fresh.households;
            let retried = op(households);
            tracing::info!(
                player = player.0.as_str(),
                cause = %err,
                action = "readdress",
                %from,
                %to,
                outcome = %retried.as_ref().map_or_else(ToString::to_string, |_| "ok".into()),
                "player moved; retried at its new address"
            );
            let r = retried?;
            Ok((
                r,
                Recovery::Readdressed {
                    player: player.clone(),
                    from,
                    to,
                },
            ))
        }
        _ => {
            tracing::warn!(
                player = player.0.as_str(),
                cause = %err,
                action = "resurvey",
                outcome = "not found at a new address",
                "player unreachable"
            );
            Err(err)
        }
    }
}

/// Run a group command addressed to the coordinator of `member`'s group.
/// UPnP fault 800 is generic (a wrong Spotify DIDL also causes it), so on
/// 800 the household's topology is refreshed and the command retried once
/// only if the coordinator really moved; otherwise the fault stands.
pub fn on_coordinator<T, R>(
    t: &T,
    households: &mut [HouseholdState],
    member: &PlayerId,
    op: impl Fn(&[HouseholdState], &PlayerId) -> Result<R, CoreError>,
) -> Result<(R, Recovery), CoreError>
where
    T: Transport + ?Sized,
{
    let coordinator = |hs: &[HouseholdState]| {
        hs.iter()
            .find_map(|h| h.coordinator_of(member).cloned())
            .ok_or_else(|| CoreError::UnknownPlayer(member.0.clone()))
    };
    let from = coordinator(households)?;
    let err = match op(households, &from) {
        Ok(r) => return Ok((r, Recovery::None)),
        Err(e @ CoreError::Proto(ProtoError::SoapFault { code: 800, .. })) => e,
        Err(e) => return Err(e),
    };
    // Refresh this household's topology from the member itself.
    let Some(h) = households.iter_mut().find(|h| h.player(member).is_some()) else {
        return Err(err);
    };
    let Some(host) = h.player(member).map(|p| p.ip) else {
        return Err(err);
    };
    let Ok(zgs) = get_zone_group_state(t, host) else {
        return Err(err);
    };
    h.apply_topology(&zgs);
    let to = coordinator(households)?;
    if to == from {
        tracing::debug!(
            member = member.0.as_str(),
            cause = %err,
            action = "refresh topology",
            outcome = "coordinator unchanged; the fault stands",
            "UPnP 800"
        );
        return Err(err);
    }
    let retried = op(households, &to);
    tracing::info!(
        member = member.0.as_str(),
        cause = %err,
        action = "retry on the new coordinator",
        from = from.0.as_str(),
        to = to.0.as_str(),
        outcome = %retried.as_ref().map_or_else(ToString::to_string, |_| "ok".into()),
        "coordinator moved"
    );
    let r = retried?;
    Ok((r, Recovery::CoordinatorMoved { from, to }))
}
