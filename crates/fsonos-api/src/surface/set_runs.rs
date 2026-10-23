//! A set of commands, one per group (`pause everywhere`, `volume all`, an
//! alias naming several rooms), carried out on every group at once (co55.9).
//! Each group's zones are captured for undo and its command sent on a thread
//! of its own, so a slow or dead player holds up only its own group: the
//! music stops everywhere else at once, where every group after it used to
//! wait out its timeouts. When two of the commands touch one zone they run
//! one after another as before, every zone captured before any changes.

use fsonos_core::snapshot::ZoneSnapshot;
use fsonos_core::{HouseholdState, actions};
use fsonos_types::PlayerId;
use std::thread;

use super::{Surface, affected, speaker_side};
use crate::execute::OutcomeDto;
use crate::failure::{ErrorCode, Failure, NoteCode};
use crate::guard::{Guard, Note};
use crate::plan::Command;

/// Groups carried out at once, at most.
const IN_FLIGHT: usize = 8;
/// Each group's thread: room for a speaker's SOAP round trips.
const STACK: usize = 8 * 1024 * 1024;

/// A set as it ran: the zones captured before its commands (snapshots, and
/// those that could not be read, with why) and each command's outcome, in
/// order. Run one after another, it ends at a failure that ends the set
/// ([`ends_set`]).
pub(super) struct SetRun {
    pub(super) snaps: Vec<ZoneSnapshot>,
    pub(super) missed: Vec<(PlayerId, String)>,
    pub(super) outcomes: Vec<Result<OutcomeDto, Failure>>,
}

/// One group's part of a set.
struct Ran {
    snaps: Vec<ZoneSnapshot>,
    missed: Vec<(PlayerId, String)>,
    outcome: Result<OutcomeDto, Failure>,
}

impl Surface {
    /// Carry `commands` out, each group's zones captured first when
    /// `capture`: all groups at once when no two commands touch one zone.
    pub(super) fn run_set(
        &self,
        households: &[HouseholdState],
        guard: &Guard<'_>,
        commands: &[Command],
        capture: bool,
    ) -> SetRun {
        let zones: Vec<Vec<PlayerId>> = commands.iter().map(|c| affected(households, c)).collect();
        let now = self.now();
        let captured = |zones: &[PlayerId]| {
            if capture {
                actions::capture_zones(&*self.transport, households, zones, now)
            } else {
                (Vec::new(), Vec::new())
            }
        };
        if commands.len() < 2 || !disjoint(&zones) {
            let mut every: Vec<PlayerId> = Vec::new();
            for zone in zones.into_iter().flatten() {
                if !every.contains(&zone) {
                    every.push(zone);
                }
            }
            let (snaps, missed) = captured(&every);
            let mut outcomes = Vec::new();
            for command in commands {
                let outcome = self.execute_following(households, guard, command);
                let ends = outcome.as_ref().is_err_and(|f| ends_set(commands.len(), f));
                outcomes.push(outcome);
                if ends {
                    break;
                }
            }
            return SetRun {
                snaps,
                missed,
                outcomes,
            };
        }
        let one = |command: &Command, zones: &[PlayerId]| {
            let (snaps, missed) = captured(zones);
            let outcome = self.execute_following(households, guard, command);
            Ran {
                snaps,
                missed,
                outcome,
            }
        };
        let one = &one;
        let mut run = SetRun {
            snaps: Vec::new(),
            missed: Vec::new(),
            outcomes: Vec::new(),
        };
        for (commands, zones) in commands.chunks(IN_FLIGHT).zip(zones.chunks(IN_FLIGHT)) {
            let ran: Vec<Ran> = thread::scope(|scope| {
                let running: Vec<_> = commands
                    .iter()
                    .zip(zones)
                    .map(|(command, zones)| {
                        thread::Builder::new()
                            .name("fsonos-set".into())
                            .stack_size(STACK)
                            .spawn_scoped(scope, move || one(command, zones))
                            // No thread to spare: this group runs here.
                            .map_err(|_| Box::new(one(command, zones)))
                    })
                    .collect();
                running
                    .into_iter()
                    .map(|started| match started {
                        Ok(running) => running
                            .join()
                            .unwrap_or_else(|panic| std::panic::resume_unwind(panic)),
                        Err(ran) => *ran,
                    })
                    .collect()
            });
            for ran in ran {
                run.snaps.extend(ran.snaps);
                run.missed.extend(ran.missed);
                run.outcomes.push(ran.outcome);
            }
        }
        run
    }
}

/// Whether no zone is in two of `zones`.
fn disjoint(zones: &[Vec<PlayerId>]) -> bool {
    zones.iter().enumerate().all(|(i, mine)| {
        zones[i + 1..]
            .iter()
            .all(|theirs| mine.iter().all(|z| !theirs.contains(z)))
    })
}

/// Whether `failure` ends a set of `commands` commands. In a set of more
/// than one, a room whose speaker failed, or that the policy denied, is
/// skipped and the rest go ahead; anything else ends it.
fn ends_set(commands: usize, failure: &Failure) -> bool {
    commands < 2 || !(speaker_side(failure) || failure.code == ErrorCode::PolicyDenied)
}

/// A set's answer from its `outcomes` ([`SetRun`]): what each group did,
/// with the groups skipped as notes; the failure that ended it; or, when no
/// group went, the first skip's failure.
pub(super) fn merged(
    households: &[HouseholdState],
    commands: &[Command],
    outcomes: Vec<Result<OutcomeDto, Failure>>,
) -> Result<OutcomeDto, Failure> {
    let mut combined = OutcomeDto {
        done: String::new(),
        changed: false,
        volume: None,
        notes: Vec::new(),
    };
    let (mut skipped, mut went) = (None, false);
    for (command, outcome) in commands.iter().zip(outcomes) {
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(failure) if !ends_set(commands.len(), &failure) => {
                let room = command
                    .addressed()
                    .and_then(|id| households.iter().find_map(|h| h.player(id)))
                    .map_or("a room", |p| p.room_name.as_str());
                combined.notes.push(Note {
                    code: NoteCode::Skipped,
                    detail: format!("{room} skipped: {}", failure.detail),
                });
                skipped.get_or_insert(failure);
                continue;
            }
            Err(failure) => return Err(failure),
        };
        went = true;
        if !combined.done.is_empty() && !outcome.done.is_empty() {
            combined.done.push_str("; ");
        }
        combined.done.push_str(&outcome.done);
        combined.changed |= outcome.changed;
        combined.volume = combined.volume.or(outcome.volume);
        combined.notes.extend(outcome.notes);
    }
    match skipped {
        Some(failure) if !went => Err(failure),
        _ => Ok(combined),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_run_at_once_only_when_no_zone_is_shared() {
        let id = |s: &str| PlayerId(s.into());
        assert!(disjoint(&[vec![id("A")], vec![id("B")], vec![id("C")]]));
        assert!(!disjoint(&[vec![id("A")], vec![id("B"), id("A")]]));
        assert!(disjoint(&[]));
        let dead = Failure::new(ErrorCode::PlayerUnreachable, "no answer");
        let refused = Failure::new(ErrorCode::InvalidArgument, "no such room");
        assert!(!ends_set(3, &dead), "a dead speaker is skipped");
        assert!(ends_set(1, &dead), "alone, its failure is the answer");
        assert!(ends_set(3, &refused));
    }
}
