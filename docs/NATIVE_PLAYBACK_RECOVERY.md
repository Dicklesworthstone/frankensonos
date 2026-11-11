# Native playback recovery

## Relative seeking

`fsonos seek ROOM +30`, `seek` over MCP and HTTP `POST /seek` with `by_secs`
use the same core operation. It reads the real current position, resolves the
offset to one absolute target, and sends one native `Seek`. A missing clock is
not treated as zero: unknown positions and an empty track fail before any seek.
Use an absolute `to_secs` when the current position cannot be obtained.

Known durations still bound the target to the last whole second before track
end; an unknown duration is not invented. Negative and extreme offsets saturate
without overflow. Successful acknowledgements retain the existing one-read,
one-write path. Absolute seeking is unchanged by this relative-seek change.

If the seek acknowledgement is lost or malformed, a single bounded read-back
batch may establish the result without repeating the offset. Both position
observations must retain the original renderer URI, queue slot and duration,
and the player must still be a coordinator. A paused or stopped track must be
at the target; a playing track may have advanced within the elapsed request
time plus one second of integer-clock rounding. Observations older than 30
seconds, unknown/transitioning states, a changed track, missing clocks, and
failed reads do not confirm success. The original write failure is retained.

These checks establish a compatible observed state, not causality or a device
transaction. Another controller can still change playback during or after the
reads, and a repeat-one loop with identical URI and queue slot has no exposed
playback-generation identity. No rollback, source change, resume, queue edit,
or automatic second seek is introduced.

Targeted checks:

```sh
cargo test --locked -p fsonos-core --lib control::seek_relative
cargo test --locked -p fsonos-core --test relative_seek_recovery_sim
```

The simulator cases use real S1/S2 loopback sockets, discarding a reply only
after the simulated renderer accepts it. They are separate from physical-Sonos
acceptance. Rust execution was unavailable in the editing environment.

## Pause, resume and stop

Ordinary native playback controls now use read-back recovery too, not just
source starts. An acknowledged control keeps its existing single-write path.
After a network failure or malformed acknowledgement, Pause can confirm paused
or stopped playback, and Stop requires stopped playback. Resume uses the shared
bounded source-start transition wait and checks coordinator ownership around
the playing-state observation. No control write is repeated by these helpers.

Direct source starts, queue starts and resume reject a confirmation that
reports `ERROR_OCCURRED`, even if the transport state still says `PLAYING` or
`TRANSITIONING`. That explicit renderer failure ends the read-back attempt
immediately and retains the original lost-reply error. Missing or unknown
status values keep their existing behavior; they are not rewritten to `OK`.

A definite Pause/701 still preserves already-inactive playback or stops an
active unpausable stream. A lost Pause reply is not treated as a 701: it cannot
escalate into Stop. An uncertain Stop in the definite-701 fallback can itself
be confirmed by a stopped-state read. Other explicit SOAP faults are returned
unchanged; failed observations keep the original write error.

These are desired-state observations, not proof that no other controller acted
or that a particular source is still selected. Existing outer coordinator and
address recovery rules remain in force when a result cannot be established.
Next and Previous use the stricter occurrence proof described below. Existing
policy, logging and source-loading checks remain shared with the callers; no
new endpoint or background polling is added.

```sh
cargo test --locked -p fsonos-core --lib control::transport_state
cargo test --locked -p fsonos-core --lib control::source_start
cargo test --locked -p fsonos-core --test transport_state_recovery_sim --test control_sim
cargo test --locked -p fsonos-core --test start_transition_sim
```

The simulator cases cover normal and malformed lost acknowledgements for all
three controls on both generations. Physical-speaker validation remains a
separate check.

The renderer-status regressions also cover lost replies for direct playback,
queue playback and resume on both simulated generations, preserving queue
contents and sending each playback write once. These new simulator cases still
require execution: the workspace build was killed with exit 137 in the existing
asupersync dependency before project checks completed.

## Next and Previous

All surfaces now use the shared core navigation operation. It sends the native
Next or Previous exactly once and preserves the renderer's behavior. Optional
reads before that write collect evidence for recovery; their failure never
prevents the requested navigation or changes its destination.

An acknowledged navigation returns immediately. A lost or malformed reply can
be confirmed only for an adjacent, distinct queue occurrence in NORMAL mode,
with a known coordinator boot epoch. A two-entry queue browse establishes the
original and expected slots, their metadata, the queue length and its UpdateID.
Read-back must preserve this exact window, the selected queue and source
metadata, the coordinator's group including bonded members, boot identity, play
mode and transport state. Position observations must agree on the expected
queue occurrence at its beginning within elapsed-time rounding. Equal track
URIs in different slots remain distinct occurrences.

Shuffle, repeat modes, a missing boot epoch, unknown clocks or transport state,
a changed queue or source, manual deep seeking, and an unchanged or unexpected
occurrence leave the original write error intact. Next near the natural end
cannot distinguish an accepted skip from automatic progression, so it cannot
confirm success. Queue boundaries and Previous that restarts the same
occurrence also retain their native acknowledgement or error. Definite SOAP
faults are returned unchanged; there is no second navigation write, recovery
loop, source selection, queue edit or playback-state change.

The normal recoverable queue path adds seven optional reads before its one
write. Successful acknowledgements and explicit SOAP faults need no read-back.
An ambiguous reply adds at most seven confirmation reads: at most fifteen
requests including the navigation itself. Initial position and mode filters
stop irrelevant evidence gathering early. Optional reads have a 750 ms
preparation budget and a 3 s confirmation budget, checked before each request;
an in-flight read still uses the transport's own timeout. Once a budget is
exhausted, no further evidence requests are started. Observations older than
30 seconds cannot establish a result.

These observations establish a compatible transition, not a transaction or
proof of causality. Another controller can still select the same occurrence
during the request or change playback after the observations.

Targeted checks:

```sh
cargo test --locked -p fsonos-core --lib control::navigation
cargo test --locked -p fsonos-core --test navigation_recovery_sim --test control
```

The simulator regressions exercise real S1/S2 loopback requests, accepted writes
with discarded or malformed replies, unchanged and conflicting observations,
explicit faults, missing evidence and bounded request counts. They remain
unexecuted in this editing session: Rust compilation was killed while compiling
the asupersync dependency, before project checking, and the execution environment
was subsequently lost. Physical-speaker acceptance remains a separate check.

## Grouping and moving music

A join, ungroup or coordinator handoff can take effect even when its SOAP
acknowledgement is lost. These operations now use fresh household topology to
check the result after a network failure or malformed acknowledgement. The
grouping write is sent once. A confirmed join lets a move continue through its
existing handoff, so the DJ and sleep timer can follow the music to its new
room and the action log can record the completed move.

Confirmation includes the room's bonded players. A join requires the intended
coordinator to retain its existing members and contain the joining room; a
leave requires the original room to lead a group of its own. A coordinator
leaving its group must also leave the other rooms together under the chosen
successor. The existing bonded-room repair remains part of that leave path.
Ambiguous membership, an unrelated regroup, or failed reads cannot establish
success. The original write failure is retained when confirmation fails.

Recovery prefers the destination or remaining coordinator, because the player
that just acted may still report stale topology. Reads use the existing
bounded regrouping wait; unreachable witnesses are not polled repeatedly.
Explicit SOAP faults keep their existing behavior, including the documented
self-join/402 leave path. These observations establish the requested layout,
not causality or a transaction against other controllers.

```sh
cargo test --locked -p fsonos-core --test grouping_recovery_sim
cargo test --locked -p fsonos-api --test dj_move_sim --test sleep_move_sim
```

The tests use real S1/S2 simulator sockets and discard an acknowledgement only
after the simulated player accepted the operation. Physical-speaker acceptance
remains a separate check.
