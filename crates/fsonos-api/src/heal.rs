//! Carrying a command out when the speakers changed under it, through
//! [`fsonos_core::heal`]: once, and only where repeating is harmless.
//!
//! * A group command refused with UPnP 800 is retried on the group's new
//!   coordinator when the topology shows the coordinator really moved
//!   ([`heal::on_coordinator`]). 800 means the player refused, so nothing
//!   ran and any group command may be retried. When the coordinator did
//!   not move, the fault stands (a wrong Spotify item also gives 800).
//! * A command whose player did not answer is retried at the player's new
//!   address, when a fresh survey shows it moved, but only if it is
//!   [`Command::repeat_safe`]: the first request may have run with only its
//!   reply lost, so a step (next, previous, a relative volume) or an append
//!   is never sent twice.
//!
//! Either recovery adds a [`NoteCode::Healed`] note to the outcome.

use fsonos_core::heal::{self, Recovery};
use fsonos_core::{CoreError, HouseholdState, control};
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::PlayerId;
use std::cell::RefCell;
use std::net::IpAddr;

use crate::execute::{OutcomeDto, execute_guarded};
use crate::failure::{ErrorCode, Failure, NoteCode};
use crate::guard::{Guard, Note};
use crate::plan::Command;

/// Carry `command` out (see the module docs). `fresh` surveys again: it is
/// called only when a repeat-safe command's player did not answer.
pub fn execute_healing<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    guard: &Guard<'_>,
    command: &Command,
    fresh: impl FnOnce() -> Option<Vec<HouseholdState>>,
) -> Result<OutcomeDto, Failure> {
    let result = on_new_coordinator(transport, households, guard, command);
    let Err(failure) = &result else {
        return result;
    };
    if failure.code != ErrorCode::PlayerUnreachable || !command.repeat_safe() {
        return result;
    }
    let Some(player) = command.addressed() else {
        return result;
    };
    let Some(from) = address(households, player) else {
        return result;
    };
    let Some(moved) = fresh() else {
        return result;
    };
    match address(&moved, player) {
        Some(to) if to != from => {
            let retried = execute_guarded(transport, &moved, guard, command.clone());
            tracing::info!(
                player = player.0.as_str(),
                %from,
                %to,
                outcome = %retried.as_ref().map_or_else(|f| f.detail.clone(), |_| "ok".into()),
                "player moved; retried at its new address"
            );
            retried.map(|outcome| {
                let room = room(&moved, player);
                healed(
                    outcome,
                    format!("{room} had moved from {from} to {to}; the command was sent there"),
                )
            })
        }
        _ => result,
    }
}

/// The command, retried once on the group's new coordinator if a UPnP 800
/// turns out to mean the coordinator moved.
fn on_new_coordinator<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    guard: &Guard<'_>,
    command: &Command,
) -> Result<OutcomeDto, Failure> {
    let Some(coordinator) = command.group_coordinator() else {
        return execute_guarded(transport, households, guard, command.clone());
    };
    // The 800 that stood, kept as reported rather than re-derived.
    let refused = RefCell::new(None);
    let mut hs = households.to_vec();
    let attempt = |hs: &[HouseholdState], to: &PlayerId| match execute_guarded(
        transport,
        hs,
        guard,
        command.clone().on_coordinator(to),
    ) {
        Err(f) if f.upnp_code == Some(800) => {
            let e = CoreError::Proto(ProtoError::SoapFault {
                code: 800,
                reason: f.detail.clone(),
            });
            *refused.borrow_mut() = Some(f);
            Err(e)
        }
        other => Ok(other),
    };
    match heal::on_coordinator(transport, &mut hs, coordinator, attempt) {
        Ok((Ok(outcome), Recovery::CoordinatorMoved { from, to })) => Ok(healed(
            outcome,
            format!(
                "{}'s group had a new coordinator ({}); the command was sent there",
                room(&hs, &from),
                room(&hs, &to)
            ),
        )),
        Ok((result, _)) => result,
        Err(e) => Err(refused.take().unwrap_or_else(|| Failure::from(e))),
    }
}

fn healed(mut outcome: OutcomeDto, detail: String) -> OutcomeDto {
    outcome.notes.push(Note {
        code: NoteCode::Healed,
        detail,
    });
    outcome
}

fn address(households: &[HouseholdState], player: &PlayerId) -> Option<IpAddr> {
    households
        .iter()
        .find_map(|h| h.player(player))
        .map(|p| p.ip)
}

fn room(households: &[HouseholdState], player: &PlayerId) -> String {
    control::locate(households, player).map_or_else(|_| player.0.clone(), |p| p.room_name.clone())
}
