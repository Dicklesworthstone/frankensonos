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

use crate::execute::{Executed, OutcomeDto};
use crate::failure::{ErrorCode, Failure, NoteCode};
use crate::guard::{Guard, Note};
use crate::plan::Command;

mod attempt;
pub(crate) use attempt::Observed;
mod volume;

/// Carry `command` out. `fresh` is used only for an eligible address recovery;
/// it is not called after a coordinator retry or a partially applied command.
pub fn execute_healing<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    guard: &Guard<'_>,
    command: &Command,
    fresh: impl FnOnce() -> Option<Vec<HouseholdState>>,
) -> Result<OutcomeDto, Failure> {
    execute_healing_with(
        transport,
        households,
        guard,
        command,
        false,
        fresh,
        |_, _| Ok(Vec::new()),
    )
    .map(|executed| executed.outcome)
}

/// The surface rechecks playback caps and continuation ownership for the
/// actual destination before a retry. This callback is never a retry itself;
/// its failure ends recovery even if it already lowered some rooms.
pub(crate) fn execute_healing_with<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    guard: &Guard<'_>,
    command: &Command,
    receipt: bool,
    fresh: impl FnOnce() -> Option<Vec<HouseholdState>>,
    prepare_retry: impl Fn(&[HouseholdState], &Command) -> Result<Vec<Note>, Failure>,
) -> Result<Executed, Failure> {
    let can_readdress = Cell::new(true);
    let volume = RefCell::new(volume::RetryVolume::new(receipt));
    let result = on_new_coordinator(
        transport,
        households,
        guard,
        command,
        &can_readdress,
        &volume,
        &prepare_retry,
    );
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
            // A DHCP change can coincide with regrouping. The old coordinator
            // is only the failed endpoint, never the room the caller named.
            let command = current_command(households, &moved, command)?;
            let mut notes = prepare_retry(&moved, &command)?;
            let destination = command.addressed().map(|target| room(&moved, target));
            let (retried, _) = observed_execute(transport, &moved, guard, command, &volume);
            tracing::info!(
                player = player.0.as_str(),
                %from,
                %to,
                outcome = %retried.as_ref().map_or_else(|f| f.detail.clone(), |_| "ok".into()),
                "player moved; retried at its new address"
            );
            retried.map(|mut executed| {
                notes.append(&mut executed.outcome.notes);
                executed.outcome.notes = notes;
                let room = room(&moved, player);
                healed(
                    executed,
                    format!(
                        "{room} had moved from {from} to {to}; the command was sent to {}",
                        destination.as_deref().unwrap_or(&room)
                    ),
                )
            })
        }
        _ => result,
    }
}

/// Rebind only the original requested room within its retained household.
/// A fresh survey must not silently adopt a different room by name, or carry
/// a household-local favorite into another household after a reset.
pub(crate) fn current_command(
    before: &[HouseholdState],
    after: &[HouseholdState],
    command: &Command,
) -> Result<Command, Failure> {
    let Some(member) = command.group_member() else {
        return Ok(command.clone());
    };
    let changed = || {
        Failure::new(
            ErrorCode::NotCoordinator,
            "the requested room's group could not be confirmed; the command was not redirected",
        )
        .with_hint("Reread the rooms, then issue the command for the intended room.")
    };
    let previous = before
        .iter()
        .find(|h| h.player(member).is_some())
        .ok_or_else(changed)?;
    let mut matches = after.iter().filter(|h| h.player(member).is_some());
    let current = matches.next().ok_or_else(changed)?;
    if matches.next().is_some()
        || previous
            .id
            .as_ref()
            .is_some_and(|old| current.id.as_ref() != Some(old))
    {
        return Err(changed());
    }
    let coordinator = current.coordinator_of(member).ok_or_else(changed)?;
    if current.player(coordinator).is_none() {
        return Err(changed());
    }
    Ok(command.clone().on_coordinator(coordinator))
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
    volume: &RefCell<volume::RetryVolume>,
) -> (Result<Executed, Failure>, bool) {
    let observed = attempt::Observed::new(transport);
    let result = volume
        .borrow_mut()
        .execute(&observed, households, guard, command);
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
    volume: &RefCell<volume::RetryVolume>,
    prepare_retry: &impl Fn(&[HouseholdState], &Command) -> Result<Vec<Note>, Failure>,
) -> Result<Executed, Failure> {
    let Some(member) = command.group_member() else {
        let (result, blocked) =
            observed_execute(transport, households, guard, command.clone(), volume);
        can_readdress.set(!blocked);
        return result;
    };
    let (refused, tried) = (RefCell::new(None), Cell::new(0));
    let attempted = Cell::new(false);
    let mut hs = households.to_vec();
    let attempt = |hs: &[HouseholdState], to: &PlayerId| {
        let retry = attempted.replace(true);
        if retry {
            // A second attempt on a new coordinator consumes the entire
            // recovery budget, including if its answer is lost afterward.
            can_readdress.set(false);
        }
        let command = if retry {
            match current_command(households, hs, command) {
                Ok(command) => command,
                Err(failure) => return Ok(Err(failure)),
            }
        } else {
            command.clone().on_coordinator(to)
        };
        let mut notes = if retry {
            match prepare_retry(hs, &command) {
                Ok(notes) => notes,
                Err(failure) => return Ok(Err(failure)),
            }
        } else {
            Vec::new()
        };
        let (result, blocked) = observed_execute(transport, hs, guard, command, volume);
        let result = result.map(|mut executed| {
            notes.append(&mut executed.outcome.notes);
            executed.outcome.notes = notes;
            executed
        });
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
    match heal::on_coordinator(transport, &mut hs, member, attempt) {
        Ok((Ok(outcome), Recovery::CoordinatorMoved { to, .. })) => Ok(healed(
            outcome,
            format!(
                "{}'s group had a new coordinator ({}); the command was sent there",
                room(&hs, member),
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
                        room(&hs, member),
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

fn healed(mut executed: Executed, detail: String) -> Executed {
    executed.outcome.notes.push(Note {
        code: NoteCode::Healed,
        detail,
    });
    executed
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ZoneRequest;
    use crate::plan::{TransportAction, plan_transport};
    use crate::zones::fixtures::{households, id};
    use fsonos_types::{HouseholdId, ZoneGroup};

    fn command(before: &[HouseholdState]) -> Command {
        plan_transport(
            before,
            &ZoneRequest {
                zone: "Kitchen@S1".into(),
            },
            TransportAction::Pause,
        )
        .unwrap()
    }

    #[test]
    fn a_fresh_survey_follows_the_requested_uuid_when_its_old_coordinator_left() {
        let before = households();
        let mut after = before.clone();
        after[0].groups = vec![
            ZoneGroup {
                coordinator: id("RINCON_KIT1"),
                members: vec![id("RINCON_KIT1")],
            },
            ZoneGroup {
                coordinator: id("RINCON_STU_L"),
                members: vec![id("RINCON_STU_L"), id("RINCON_DEN")],
            },
        ];
        // Neither a renamed target nor another household's Kitchen changes
        // the UUID retained by the original plan.
        after[0].players[1].room_name = "New Kitchen".into();
        let rebound = current_command(&before, &after, &command(&before)).unwrap();
        assert_eq!(rebound.group_coordinator(), Some(&id("RINCON_KIT1")));
        assert_eq!(rebound.group_member(), Some(&id("RINCON_KIT1")));
        let another_handover = rebound.on_coordinator(&id("RINCON_STU_L"));
        assert_eq!(another_handover.group_member(), Some(&id("RINCON_KIT1")));
    }

    #[test]
    fn a_known_household_must_still_be_confirmed_after_survey() {
        let before = households();
        let command = command(&before);
        for household in [None, Some(HouseholdId("RESET_HOUSEHOLD".into()))] {
            let mut after = before.clone();
            after[0].id = household;
            let failure = current_command(&before, &after, &command).unwrap_err();
            assert_eq!(failure.code, ErrorCode::NotCoordinator);
        }
        assert!(current_command(&before, &before, &command).is_ok());
    }

    #[test]
    fn a_missing_ambiguous_or_unusable_target_is_not_redirected() {
        let before = households();
        let command = command(&before);
        let mut absent = before.clone();
        absent[0]
            .players
            .retain(|player| player.id != id("RINCON_KIT1"));
        let mut ambiguous = before.clone();
        ambiguous.push(before[0].clone());
        let mut groupless = before.clone();
        groupless[0].groups.clear();
        let mut leader_absent = before.clone();
        leader_absent[0]
            .players
            .retain(|player| player.id != id("RINCON_DEN"));
        for after in [absent, ambiguous, groupless, leader_absent] {
            let failure = current_command(&before, &after, &command).unwrap_err();
            assert_eq!(failure.code, ErrorCode::NotCoordinator);
        }
    }
}
