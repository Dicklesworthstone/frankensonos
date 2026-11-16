# Command recovery does not restart completed stages

A source start or playlist replacement contains several speaker requests. A
refusal or lost reply from a later request does not roll back the earlier ones.
The outer command-recovery path now observes those requests as they execute,
including the lower-level source helper's read-back after a lost reply.

An initial UPnP 800 can still trigger the existing topology refresh and retry
on a confirmed new coordinator. A single repeat-safe state write can still
recover at a player's newly discovered address. Coordinator and address
recovery share one retry budget: a failed coordinator retry cannot dispatch a
third command to the original player.

A failed read-only preflight can also recover the player's address for a
relative command, such as next, previous, seek or a volume adjustment. The
per-attempt observer must establish that no mutation may have taken effect;
the same one-retry budget and original-room checks still apply. Once an
uncertain relative write has been dispatched, it is never repeated at a new
address, even when its optional read-back evidence is unavailable.

Group recovery retains the UUID of the room the caller requested, separately
from the coordinator that first received the command. If that former leader
joins another group, recovery follows the requested room's current coordinator.
It does not follow the old leader into its new audience. A simultaneous address
change uses the same retained room and household identity; an ambiguous or
unconfirmed destination requires a fresh request.

Before retrying a source start or resume, the surface checks playback caps for
the actual destination group, including rooms that joined after the original
plan. Successful playback history is attributed to that destination. Recovery
does not add another retry or permit replay of a partially completed operation.

If recovery reaches a different set of rooms or physical players, the action
log records an undo boundary: its original snapshot cannot safely restore the
new audience. Neither ordinary nor forced undo can use that obsolete snapshot
or reach past the action. The boundary survives an uncertain failed retry,
including a failed group within a larger set command. A coordinator election
or address change with the same household and members remains undoable.

Once an earlier write may have taken effect, the outer path preserves the
original failure and reports that earlier steps may have applied. It does not
resend the source selection, queue edits or playback command from the start.
Known reads do not count as writes, nor does the `SnapshotGroupVolume` that
only prepares a group volume write. Typed and raw SOAP 800 refusals are both
recognized; other write replies, including lost replies and Leave/402, are
conservatively treated as possible effects.

This is not transaction rollback, household isolation, or an assertion that
all lower-level orchestration is atomic. Inspect playback and the action log
before issuing another command after a partial failure. The original error
code and UPnP cause remain available. An error's transient `retryable` category
is not permission to replay a partially completed mutation.

Regressions: `cargo test --locked -p fsonos-api --lib heal::` and
`cargo test --locked -p fsonos-api --test command_recovery_sim --test heal_sim
--test recovery_target_sim --test recovery_undo_sim`.
Unit tests cover progress classification, raw and typed faults, uncertain
writes and transport forwarding. The command-recovery simulator tests exercise
late coordinator changes, source success followed by a lost Play reply,
one shared recovery budget, positive coordinator/address recovery, and
read-back completion after an accepted write loses its acknowledgement.
Target and undo regressions cover a former leader joining an unrelated group,
simultaneous address changes, new members' playback caps, actual-target history,
and undo boundaries after membership changes. Physical-speaker acceptance is
separate. Recovery only starts after an eligible failure; these checks do not
make the initial request and concurrent topology changes atomic.

## The addressed player must own the requested group

Direct-source starts, queue starts and resume verify the requested coordinator
at the addressed endpoint before their first playback write. Silent loads and
their prerequisite pause use the same check. A household-wide topology listing
the coordinator somewhere else is insufficient: the coordinator's own advertised
address must match the endpoint, and the topology must be coherent. An address
now belonging to another group therefore cannot receive a command intended for
the old player, even when it already has the same source or queue track.

A failed initial ownership check reports `NOT_COORDINATOR` without inventing a
speaker SOAP fault. The surface can use its existing single recovery budget to
refresh topology and resolve the originally requested room again. Its existing
policy, actual-audience and progress checks still apply. A mismatch detected
after a write is an uncertain result; it cannot authorize restarting completed
stages. Lost-reply confirmation also requires this exact endpoint ownership.

These are bounded observations, not an atomic lock on another controller or on
address assignment. A change after the last check remains possible. Simulator
regressions exercise stale addresses in both S1 and S2, acknowledged and lost
replies, silent loading, and successful recovery to the intended room.

## A lost daemon reply does not authorize another write

The CLI and its forwarded stdio MCP tools report `OUTCOME_UNCERTAIN` when a
POST, PUT, or DELETE loses its reply, returns an incomplete HTTP body, or
supplies an unreadable result. This includes untyped proxy errors: the daemon
may already have applied the action before a proxy times out. A valid JSON
reply that does not match an MCP tool's expected result also leaves the action
unconfirmed.

These failures are not retryable. Inspect the current playback, queue, saved
configuration, and action log before deciding on another action. Replaying a
queue addition, a skip, a relative volume change, or a schedule creation could
apply it twice; switching to direct mode does not recover the first result.
Ordinary GET failures retain their read-only retry behavior. A complete typed
daemon refusal preserves its original code, hint, and retryability, including
a `NOT_READY` response that refused admission before execution.

Loopback regressions in `fsonos-cli`'s `remote::tests` cover lost replies,
malformed JSON, incomplete length/chunk framing, proxy errors, typed refusals,
and a single dispatched request. `fsonos-mcp`'s daemon forwarding tests cover
malformed mutation results and preserve authoritative daemon failures.

## A volume retry keeps its first allowance

For absolute room and group volumes, the shared recovery path remembers the
first policy-bounded target and its clamp notes. A lost acknowledgement after
a successful increase must not grant another max-step increase when the same
player is rediscovered at a different address. The retry can only retain or
lower that first target; it rechecks the same policy against current levels.
A room lowered by another controller, a changed group, or a newly applicable
quiet-hours cap can therefore impose a stricter limit. The first clamp remains
visible in a successful recovery's notes. Uncapped CLI behavior is unchanged.

This allowance is scoped to one call; it does not change future deliberate
requests, add a retry, or make relative adjustments safe to replay. Run
`cargo test --locked -p fsonos-api --test recovery_volume_sim` for S1/S2 room,
group, intervening-lower-level and uncapped-client cases.
