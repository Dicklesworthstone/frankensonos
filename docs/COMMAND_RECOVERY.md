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

Named transport commands verify the requested coordinator at the addressed
endpoint before their first playback write: direct-source and queue starts,
resume, pause, stop, next, previous, and both absolute and relative seeks.
Silent loads and their prerequisite pause use the same check. Navigation and
seek verify ownership after their preparatory reads, immediately before the
write. A household-wide topology listing the coordinator somewhere else is
insufficient: coherent topology and the coordinator's own endpoint identity
must agree. A stale advertised address needs the fresh description proof below. An address now belonging to another group
therefore cannot receive a command intended for the old player, even when it
already has the same source or queue track.

A failed initial ownership check reports `NOT_COORDINATOR` without inventing a
speaker SOAP fault. The surface can use its existing single recovery budget to
refresh topology and resolve the originally requested room again. Its existing
policy, actual-audience and progress checks still apply. A mismatch detected
after a write is an uncertain result; it cannot authorize restarting completed
stages. Lost-reply confirmation also requires this exact endpoint ownership.

Successful acknowledgements retain the same ownership boundary. Before Play
after selecting a direct source, the controller rereads that source and the
coordinator. Supplied source metadata must remain equivalent XML, preserving
service/account descriptors and unknown extensions while accepting namespace
prefix, attribute-order and indentation changes. An unspecified metadata
document does not require the speaker to return an empty document. A source
replacement, regrouping or failed prerequisite read stops before Play and
reports `OUTCOME_UNCERTAIN` for the already completed source selection.

Queue starts retain the requested occurrence and queue revision before selecting
the queue. Each later stage verifies that original observation and the preceding
stage before changing the play mode, seeking or playing. Ordered starts also
confirm NORMAL mode before Play; an acknowledged mode change may continue
while playing or transitioning, while silent loading still requires inactivity.
A missing occurrence fails before the source
is interrupted; a later queue replacement, reorder, selected-track change or
ownership loss cannot authorize a command against the new music. These bounded
reads apply to acknowledged replies as well as lost replies, without repeating
completed writes. A change after the final read remains possible.

Pause and stop confirm a lost reply only when the expected coordinator still
owns the endpoint before and after the state observation. A stopped or paused
unrelated player cannot confirm success. A definite Pause/701 refusal can fall
back to Stop only after the same ownership checks around an active-state read;
an unverifiable destination preserves the original refusal without another
write. The anonymous `pause_at` helper has no player identity to assert.

These are bounded observations, not an atomic lock on another controller or on
address assignment. A change after the last check remains possible. Simulator
regressions exercise stale addresses in both S1 and S2, acknowledged and lost
replies, silent loading, and successful recovery to the intended room. The
navigation recovery matrix covers moved and reused addresses, next/previous,
and absolute/relative seeks. A recovered relative seek reads the intended
player's current position again; it does not reuse the other player's clock.

Focused regressions: `cargo test --locked -p fsonos-core --test control
--test control_endpoint_sim --test navigation_recovery_sim` and
`cargo test --locked -p fsonos-api --test preflight_recovery_sim
--test navigation_preflight_sim --test command_recovery_sim --test heal_sim`.
Acknowledged source/queue-stage regressions are in `source_start_sim`,
`queue_start_sim` and `ordered_mode_recovery_sim` in `fsonos-core`, with the outer
retry boundary exercised by `fsonos-api`'s `command_recovery_sim`.

## Recover while a player still advertises its old address

Discovery can identify a player's new DHCP address before its own topology
advertises that address. The shared command proof now accepts this bounded
case: coherent topology still contains the requested physical player, no other
member or direct satellite claims the contacted address, and a fresh GET of
that address's fixed device-description path identifies the exact same renderer.

The description read is bracketed by coherent topology reads. Comparison repairs
only the requested player's Location; its identity, group, coordinator, boot
sequence, membership and all other addresses must remain unchanged. The advertised
address may stay old or catch up to the contacted address. A third address means
the player moved again and is refused. Missing, duplicate, conflicting or
unsupported nested-player evidence cannot authorize repair.

Device descriptions require one unambiguous root UDN. SOAP, descriptions and
GENA refuse HTTP redirects, so a different endpoint cannot lend its identity or
receive a control request. Artwork byte reads retain their redirect support.
Ordinary matching topology requires no additional description read. A fallback
adds one description GET and one topology read, and the evidence is never
reused for a later command.

Direct-source and queue starts, their confirmations, scene/source restoration,
and own-player cleanup use the same repaired binding. Group roles and the
paired grouping checks remain unchanged; identifying a group member never
promotes it to coordinator. Peer-witness reads retain their existing checks.
Repair adds no retry budget and cannot replay a possibly completed write.

The S1/S2 regressions in `endpoint_readdressing_sim` cover DHCP recovery,
direct and ordered queue starts, stale Location catch-up, unavailable or
ambiguous descriptions, reused addresses, conflicting members, coordinator
roles, and topology changes during verification. The protocol redirect tests
use real localhost servers and assert that redirected endpoints receive no
speaker control or description request.

~~~bash
cargo test --locked -p fsonos-core --test endpoint_readdressing_sim
cargo test --locked -p fsonos-proto --lib net::redirect_tests
cargo test --locked -p fsonos-proto --lib description::tests
~~~

## Grouping keeps both physical destinations

A join verifies the moving room's endpoint and the destination coordinator's
endpoint before changing either room. Both fresh household views must still
identify the requested destination as a coordinator. Cross-household joins,
reassigned addresses, incoherent topology and evidence belonging to another
known household cannot authorize a grouping write. Leaving likewise chooses
its handoff from validated current topology without adopting a replacement
player from the old address's reply.

An uncertain join or leave is observed through the existing bounded household
witness path, without repeating its write. Each witness must identify itself
at the address contacted. Stereo-room cleanup retains the original bonded
players, and checks their endpoints before reuniting them. S1's stale self-view
can still be confirmed by another player in the household.

Pre-write identity failures retain the existing bounded address recovery.
Regressions in `grouping_endpoint_sim` cover both S1 and S2, real address reuse
followed by successful recovery, malformed and foreign topology, lost replies,
and successful stereo joins and handoffs preserving queue position. Run
`cargo test --locked -p fsonos-core --test grouping_endpoint_sim
--test grouping_sim --test grouping_recovery_sim --test sleep_leave_cleanup_sim`.

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

## Volume and mute target verified room endpoints

Ordinary room volume and mute commands verify that the requested physical
player still owns the addressed endpoint. A room remains independently
controllable when another room coordinates its group; the identity check does
not redirect it to that coordinator. Policy level reads verify the same room
before using its volume as evidence, and the setter checks again before writing.
Automatic playback-cap reductions use these guarded room setters as well.

Group volume verifies the requested coordinator before `SnapshotGroupVolume`
and again before the actual absolute or relative volume write. The snapshot
prepares the room balance without changing the audible volume. An ownership
change during that preparation stops the write and remains eligible for the
existing bounded recovery. A dispatched relative volume write with an uncertain
reply is still never repeated.

The `volume_endpoint_sim` tests in `fsonos-core` cover room identity, grouped
noncoordinators, stale addresses and changes during group-volume preparation.
The API's `volume_endpoint_recovery_sim` regressions cover moved/reused
addresses, policy evidence, capped unmute and accepted relative writes whose
acknowledgements are lost. Run them with
`cargo test --locked -p fsonos-core --test volume_endpoint_sim` and
`cargo test --locked -p fsonos-api --test volume_endpoint_recovery_sim
--test recovery_volume_sim --test preflight_recovery_sim`.

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
