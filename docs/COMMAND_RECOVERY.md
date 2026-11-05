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
`cargo test --locked -p fsonos-api --test command_recovery_sim --test heal_sim`.
Unit tests cover progress classification, raw and typed faults, uncertain
writes and transport forwarding. The command-recovery simulator tests exercise
late coordinator changes, source success followed by a lost Play reply,
one shared recovery budget, positive coordinator/address recovery, and
read-back completion after an accepted write loses its acknowledgement.
Physical-speaker acceptance is separate.

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
