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

use super::{Surface, affected, dj_targets, recovery, speaker_side};
use crate::execute::{ControlBatchDto, ControlResultDto, ControlTargetDto, OutcomeDto};
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
    pub(super) queue_undo: Vec<actions::QueueUndo>,
    /// A recovered attempt reached rooms outside its captured audience.
    pub(super) unsafe_undo: bool,
}

/// One group's part of a set.
struct Ran {
    snaps: Vec<ZoneSnapshot>,
    missed: Vec<(PlayerId, String)>,
    outcome: Result<OutcomeDto, Failure>,
    queue_undo: Option<actions::QueueUndo>,
    unsafe_undo: bool,
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
        pinned_dj: bool,
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
            return self.run_sequential(households, guard, commands, snaps, missed, pinned_dj);
        }
        let one = |command: &Command, zones: &[PlayerId]| {
            let (snaps, missed) = captured(zones);
            let recovery = recovery::Audience::new(households, command);
            let mut queue_undo = None;
            let outcome = if pinned_dj || dj_targets::is_set(commands) {
                self.execute_dj_member(households, guard, command)
            } else {
                self.execute_following(households, guard, command, &recovery, &mut queue_undo)
            };
            let unsafe_undo = recovery.unsafe_undo(&outcome);
            Ran {
                snaps,
                missed,
                outcome,
                queue_undo,
                unsafe_undo,
            }
        };
        let one = &one;
        let mut run = SetRun {
            snaps: Vec::new(),
            missed: Vec::new(),
            outcomes: Vec::new(),
            queue_undo: Vec::new(),
            unsafe_undo: false,
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
                run.queue_undo.extend(ran.queue_undo);
                run.unsafe_undo |= ran.unsafe_undo;
            }
        }
        run
    }

    fn run_sequential(
        &self,
        households: &[HouseholdState],
        guard: &Guard<'_>,
        commands: &[Command],
        snaps: Vec<ZoneSnapshot>,
        missed: Vec<(PlayerId, String)>,
        pinned_dj: bool,
    ) -> SetRun {
        let mut outcomes = Vec::new();
        let mut queue_undo = Vec::new();
        let mut unsafe_undo = false;
        for command in commands {
            let recovery = recovery::Audience::new(households, command);
            let mut restored_queue = None;
            let outcome = if pinned_dj || dj_targets::is_set(commands) {
                self.execute_dj_member(households, guard, command)
            } else {
                self.execute_following(households, guard, command, &recovery, &mut restored_queue)
            };
            queue_undo.extend(restored_queue);
            unsafe_undo |= recovery.unsafe_undo(&outcome);
            let ends = outcome
                .as_ref()
                .is_err_and(|f| !dj_targets::is_set(commands) && ends_set(commands.len(), f));
            outcomes.push(outcome);
            if ends {
                break;
            }
        }
        SetRun {
            snaps,
            missed,
            outcomes,
            queue_undo,
            unsafe_undo,
        }
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

/// Keep every actual result after a batch has run. An error from one worker
/// cannot erase another worker's successful command. A single command and an
/// entirely failed batch retain their existing error responses.
pub(super) fn merged(
    households: &[HouseholdState],
    commands: &[Command],
    outcomes: Vec<Result<OutcomeDto, Failure>>,
) -> Result<OutcomeDto, Failure> {
    let dj_set = dj_targets::is_set(commands);
    let mut combined = OutcomeDto {
        done: String::new(),
        changed: false,
        volume: None,
        notes: Vec::new(),
        batch: None,
    };
    let (mut skipped, mut terminal, mut went) = (None, None, false);
    let mut results = Vec::with_capacity(commands.len());
    let mut outcomes = outcomes.into_iter();
    for command in commands {
        let (household, target, zone) = target_identity(households, command);
        let mut receipt = ControlTargetDto {
            household,
            target,
            zone,
            result: ControlResultDto::NotAttempted,
        };
        let outcome = match outcomes.next() {
            Some(Ok(outcome)) => outcome,
            Some(Err(failure)) => {
                let detail = format!(
                    "{} skipped: {} ({}). Inspect this target before another command; do not replay the batch",
                    dj_targets::label(households, command),
                    failure.detail,
                    failure.code.as_str(),
                );
                combined.notes.push(Note {
                    code: NoteCode::Skipped,
                    detail,
                });
                receipt.result = ControlResultDto::Failed {
                    error: failure.body(),
                };
                results.push(receipt);
                if !dj_set && ends_set(commands.len(), &failure) {
                    terminal.get_or_insert_with(|| failure.clone());
                }
                skipped.get_or_insert(failure);
                continue;
            }
            None => {
                combined.notes.push(Note {
                    code: NoteCode::Skipped,
                    detail: format!(
                        "{} was not attempted after an earlier failure; do not replay the batch",
                        dj_targets::label(households, command),
                    ),
                });
                results.push(receipt);
                continue;
            }
        };
        went = true;
        if !combined.done.is_empty() && (dj_set || !outcome.done.is_empty()) {
            combined.done.push_str("; ");
        }
        if dj_set {
            combined
                .done
                .push_str(&dj_targets::label(households, command));
            combined.done.push_str(": ");
        }
        combined.done.push_str(&outcome.done);
        combined.changed |= outcome.changed;
        combined.volume = combined.volume.or(outcome.volume);
        receipt.result = ControlResultDto::Succeeded {
            done: outcome.done,
            changed: outcome.changed,
            volume: outcome.volume,
            notes: outcome.notes.clone(),
        };
        results.push(receipt);
        combined.notes.extend(outcome.notes);
    }
    if commands.len() > 1 {
        combined.batch = Some(ControlBatchDto {
            complete: results.iter().all(|target| target.result.complete()),
            results,
        });
    }
    match skipped {
        Some(_) if !went && dj_set => Err(Failure::new(
            ErrorCode::OutcomeUncertain,
            combined.notes.iter().map(|note| note.detail.as_str()).collect::<Vec<_>>().join("; "),
        )
        .with_hint("No group returned a successful DJ result. Read the groups and action log; do not replay the batch.")),
        Some(failure) if !went => Err(terminal.unwrap_or(failure)),
        _ => Ok(combined),
    }
}

fn target_identity(households: &[HouseholdState], command: &Command) -> (String, String, String) {
    let Some(target) = command.addressed() else {
        return (String::new(), String::new(), String::new());
    };
    let labels = fsonos_core::rooms::household_labels(households);
    households
        .iter()
        .enumerate()
        .find_map(|(index, house)| {
            house.player(target).map(|player| {
                (
                    labels[index].clone(),
                    target.0.clone(),
                    player.room_name.clone(),
                )
            })
        })
        .unwrap_or_else(|| (String::new(), target.0.clone(), target.0.clone()))
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

    fn pause(id: &str) -> Command {
        Command::Transport {
            member: PlayerId(id.into()),
            coordinator: PlayerId(id.into()),
            action: crate::plan::TransportAction::Pause,
        }
    }

    fn accepted() -> Result<OutcomeDto, Failure> {
        Ok(OutcomeDto {
            done: "paused the group".into(),
            changed: true,
            volume: None,
            notes: Vec::new(),
            batch: None,
        })
    }

    #[test]
    fn a_terminal_failure_cannot_discard_other_workers_successful_results() {
        let result = merged(
            &[],
            &[pause("first"), pause("changed"), pause("last")],
            vec![
                accepted(),
                Err(Failure::new(
                    ErrorCode::QueueChanged,
                    "queue changed before dispatch",
                )),
                accepted(),
            ],
        )
        .unwrap();
        assert!(result.changed);
        assert!(!result.complete());
        let batch = result.batch.unwrap();
        assert!(!batch.complete);
        assert_eq!(batch.results.len(), 3);
        assert_eq!(batch.results[0].target, "first");
        assert_eq!(batch.results[2].target, "last");
        assert!(matches!(
            batch.results[0].result,
            ControlResultDto::Succeeded { .. }
        ));
        assert!(matches!(
            batch.results[2].result,
            ControlResultDto::Succeeded { .. }
        ));
        assert!(
            matches!(&batch.results[1].result, ControlResultDto::Failed { error } if error.code == ErrorCode::QueueChanged)
        );
    }

    #[test]
    fn an_ended_sequential_batch_distinguishes_unattempted_targets() {
        let failure = Failure::new(ErrorCode::Internal, "the command could not finish");
        let result = merged(
            &[],
            &[pause("first"), pause("failed"), pause("unattempted")],
            vec![accepted(), Err(failure)],
        )
        .unwrap();
        assert_eq!(result.incomplete_targets(), 2);
        let batch = result.batch.unwrap();
        assert!(matches!(
            batch.results[2].result,
            ControlResultDto::NotAttempted
        ));
    }

    #[test]
    fn single_target_and_all_failed_batches_keep_their_error_contract() {
        let failure = Failure::new(ErrorCode::UpnpFault, "the speaker refused");
        assert_eq!(
            merged(&[], &[pause("one")], vec![Err(failure.clone())]),
            Err(failure.clone())
        );
        assert_eq!(
            merged(
                &[],
                &[pause("one"), pause("two")],
                vec![Err(failure.clone()), Err(failure.clone())]
            ),
            Err(failure),
        );
        let one = merged(&[], &[pause("one")], vec![accepted()]).unwrap();
        assert!(one.complete());
        assert!(one.batch.is_none());
        let all = merged(
            &[],
            &[pause("one"), pause("two")],
            vec![accepted(), accepted()],
        )
        .unwrap();
        assert!(all.complete());
        assert!(all.batch.unwrap().complete);
    }
}
