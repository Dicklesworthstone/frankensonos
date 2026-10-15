//! Carrying a command out when the speakers changed under it, through
//! [`fsonos_core::heal`]: at most one recovery, without replaying prior steps.
//!
//! A group's initial UPnP 800 refusal can recover on a confirmed new
//! coordinator. A repeat-safe command can recover at a player's new address.
//! Neither path restarts a command after earlier speaker writes may have
//! taken effect: a fault from a later step does not roll the earlier ones back.
//! Coordinator recovery and address recovery share one retry budget.

use fsonos_core::heal::{self, Recovery};
use fsonos_core::{CoreError, HouseholdState, control};
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::PlayerId;
use std::cell::{Cell, RefCell};
use std::net::IpAddr;

use crate::execute::{OutcomeDto, execute_guarded};
use crate::failure::{ErrorCode, Failure, NoteCode};
use crate::guard::{Guard, Note};
use crate::plan::Command;

mod attempt;

/// Carry `command` out. `fresh` is used only for an eligible address recovery;
/// it is not called after a coordinator retry or a partially applied command.
pub fn execute_healing<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    guard: &Guard<'_>,
    command: &Command,
    fresh: impl FnOnce() -> Option<Vec<HouseholdState>>,
) -> Result<OutcomeDto, Failure> {
    let can_readdress = Cell::new(true);
    let result = on_new_coordinator(transport, households, guard, command, &can_readdress);
    let Err(failure) = &result else {
        return result;
    };
    if !can_readdress.get()
        || failure.code != ErrorCode::PlayerUnreachable
        || !command.repeat_safe()
    {
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
            let (retried, _) = observed_execute(transport, &moved, guard, command.clone());
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

/// Observe below orchestration, including its read-back paths. A network or
/// malformed reply may follow a real effect, so it also counts as a write.
/// A final unconfirmed operation is excluded: an initial refusal can still heal,
/// as can a single repeat-safe write whose player moved before answering.
fn observed_execute<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    guard: &Guard<'_>,
    command: Command,
) -> (Result<OutcomeDto, Failure>, bool) {
    let observed = attempt::Observed::new(transport);
    let result = execute_guarded(&observed, households, guard, command);
    let prior = observed.preceding_writes();
    let blocked = result.is_err() && prior > 0;
    let result = result.map_err(|failure| {
        if blocked {
            failure.with_hint(format!(
                "{prior} earlier speaker step(s) may have taken effect; this command was not \
                 repeated. Inspect playback and the action log before issuing a new command."
            ))
        } else {
            failure
        }
    });
    (result, blocked)
}

/// Retry an initial 800 on a genuinely new coordinator. A late 800 stays a
/// command failure, not a request to restart all of its successful stages.
fn on_new_coordinator<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    guard: &Guard<'_>,
    command: &Command,
    can_readdress: &Cell<bool>,
) -> Result<OutcomeDto, Failure> {
    let Some(coordinator) = command.group_coordinator() else {
        let (result, blocked) = observed_execute(transport, households, guard, command.clone());
        can_readdress.set(!blocked);
        return result;
    };
    let (refused, tried) = (RefCell::new(None), Cell::new(0));
    let attempted = Cell::new(false);
    let mut hs = households.to_vec();
    let attempt = |hs: &[HouseholdState], to: &PlayerId| {
        if attempted.replace(true) {
            // A second attempt on a new coordinator consumes the entire
            // recovery budget, including if its answer is lost afterward.
            can_readdress.set(false);
        }
        let (result, blocked) =
            observed_execute(transport, hs, guard, command.clone().on_coordinator(to));
        if blocked {
            can_readdress.set(false);
            return Ok(result);
        }
        match result {
            Err(f) if f.upnp_code == Some(800) => {
                tried.set(tried.get() + 1);
                let e = CoreError::Proto(ProtoError::SoapFault {
                    code: 800,
                    reason: f.detail.clone(),
                });
                *refused.borrow_mut() = Some(f);
                Err(e)
            }
            other => Ok(other),
        }
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
        Err(e) => Err(match refused.take() {
            Some(refused) if tried.get() > 1 => Failure {
                upnp_code: refused.upnp_code,
                ..Failure::new(
                    ErrorCode::NotCoordinator,
                    format!(
                        "{}'s group changed while the command ran: its new coordinator \
                         refused it too ({})",
                        room(&hs, coordinator),
                        refused.detail
                    ),
                )
                .with_hint(
                    "Retry once the group settles; if it is refused again, the speakers \
                     may not play what was asked.",
                )
            },
            refused => refused.unwrap_or_else(|| Failure::from(e)),
        }),
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
