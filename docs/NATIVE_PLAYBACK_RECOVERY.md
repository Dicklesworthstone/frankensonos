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
one-write path.

If the seek acknowledgement is lost or malformed, a single bounded read-back
batch may establish the result without repeating the offset. Both position
observations must retain the original renderer URI, queue slot and duration,
and the endpoint must still identify the original coordinator. A paused or
stopped track must be at the target; a playing track may have advanced within the elapsed request
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

## Absolute seeking

An absolute seek also sends its native request once. Optional preparation
retains the current track occurrence, duration, source, coordinator address,
physical group and boot identity. Queue playback additionally retains one
native queue entry and its revision, independent of the full queue's length.
Unavailable preparation leaves ordinary absolute seeking available, including
when the current clock is unknown, but cannot authorize lost-reply recovery.

An ambiguous acknowledgement can be confirmed only against that retained
observation and the same bounded position-clock rules as relative seeking.
Source changes, queue edits, adjacent identical tracks, renderer errors and
failed reads preserve the original write failure. Definite SOAP refusals are
never turned into success by an unrelated matching clock. Preparation admits
reads for at most 750 ms, confirmation for three seconds; an in-flight request
keeps its transport timeout, and evidence older than 30 seconds is rejected.

Absolute seeks are no longer treated as safe address-recovery retries: a time
is relative to the selected track, and replaying it after rediscovery could
seek a later occurrence. The shared CLI/HTTP/MCP command path returns the
uncertain failure instead. An explicit initial coordinator refusal retains
the existing single coordinator-recovery allowance.

The S1/S2 cases in `relative_seek_recovery_sim` cover accepted and lost replies,
playing and paused clocks, duplicate occurrences, source/queue changes,
unavailable preparation and definite refusals. `fsonos-api`'s
`seek_recovery_sim` checks that an unconfirmed absolute seek never enters
address rediscovery or sends another seek. These added tests have not executed:
the workspace dependency build was killed, and the execution host subsequently
became unavailable.

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

## Spotify track account recovery

Track playback, silent loading, queue-next and queue-append can recover when a
household's most common favorite still names a stale Spotify account. Favorites
supply ranked, distinct, complete rendering templates; after a definite initial
SOAP 800 refusal, the single recovery attempt prefers a different observed
template. Account fields are never mixed, and each household supplies its own
favorites. Queue recovery compares the actual URI and DIDL so an equivalent
rejected payload cannot consume the retry merely because unused flags differ.

The original template remains the one transient retry when no alternative
exists. Lost replies and failures after an accepted source or queue write retain
the existing observation and no-replay boundaries. There is no account guessing,
new credential source, cross-household cache or unbounded retry loop.

The `track_favorites_sim` regression cases exercise direct playback, silent
loading, queue-next and bound queue-append on both simulated generations,
including missing alternatives, lost acknowledgements and later read-back
refusals. Protocol and core unit tests cover coherent ranking and equivalent
queue payloads. Run these focused localhost regressions with:

```sh
cargo test --locked -p fsonos-proto --lib ranked_track
cargo test --locked -p fsonos-core --lib control::spotify_source::tests
cargo test --locked -p fsonos-core --test track_favorites_sim
```

Physical-speaker acceptance remains separate from these simulator checks.

## Direct source selection after a playback refusal

Direct source playback and silent loading retain the completed source-selection
stage when reporting a later failure. Once `SetAVTransportURI` has succeeded
or its lost reply has been confirmed, a failed `Play` or silent-load confirmation
returns an uncertain overall operation. The original cause remains in the
error, but neither Spotify account refresh nor coordinator/address recovery can
replay that source change. Another controller's newer source therefore stays
selected, including when the later failure is an explicit SOAP error 800.

An initial `SetAVTransportURI` refusal still permits the existing single
renderer-account refresh. Successful playback keeps its two-write path, and
read-back can still confirm a lost acknowledgement without resending it.
Failure is not rollback: the requested source may already be selected, and
the caller should inspect current playback before an explicit new request.

```sh
cargo test --locked -p fsonos-core --test control --test source_start_sim --test source_loading_sim
```

The added S1/S2 simulator cases exercise a one-shot Play/800 refusal and faults
at every silent-load confirmation stage, including an intervening listener
source change. They verify one source selection, no repeated account learning,
preserved queues and the listener's continued playback. Physical-speaker
acceptance remains separate from these localhost regressions.

## Queue replacement after a playback refusal

Album, playlist and container-favorite playback stages new entries after the
existing queue, verifies them, and only then removes the old prefix. Once that
replacement is committed, a failure selecting the queue, seeking its first
entry or starting playback ends the operation. Even a definite SOAP error 800
is reported as an uncertain overall replacement; it cannot trigger another
append/removal through Spotify renderer-account recovery or address healing.
An intervening queue edit or source selection therefore remains untouched by
a replay of that completed replacement.

A definite rejection of the initial enqueue can still use the existing bounded
renderer-account refresh because the replacement has not been committed. A
later failure is not a rollback: the queue may already have changed, and the
original prefix is not automatically restored. Inspect the current queue and
source before explicitly requesting another start.

Preparation of an optional undo receipt also performs speaker reads. The
replacement rechecks its original source, group and queue after those reads,
before pausing an active source or appending the replacement. A takeover or
queue edit during preparation therefore leaves the listener's selection intact.
After staging, source and group checks run again after receipt confirmation
and the removal helper's own coordinator read, immediately before prefix
removal. A silent load also requires playback to remain inactive. A failed
check preserves the old prefix; the staged suffix can remain and the result
requires inspection, with no automatic rollback or replay.

```sh
cargo test --locked -p fsonos-core --test spotify_replacement_sim
cargo test --locked -p fsonos-core --test spotify_track_batch_sim
cargo test --locked -p fsonos-core --lib queue_source::ownership
```

The regression cases exercise S1/S2 simulator transports, normal and receipt
entry points, and late URI/Seek/Play refusals with intervening queue edits or
source takeovers. They were added without a usable Rust execution environment;
CI and physical-speaker acceptance remain separate checks.

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

## Queue operations retain the actual coordinator endpoint

A household-wide topology response can come from any of its players. Queue
replacement, explicit track batches, Spotify insertion, native-favorite
insertion and removal now require the intended coordinator's advertised
address to match the endpoint receiving the command. The shared check also
rejects incoherent topology and bridge identities. A matching queue revision
or identical contents on another player do not establish ownership.

Replacement observations retain that address through source transitions.
Native-favorite insertion and removal check endpoint identity again after
their write: a changed or unreadable result stays uncertain and is not
replayed. These observations detect address reuse and ownership changes;
they do not lock out another controller between the final read and write.

The `fsonos-core` `queue_source::ownership` simulator cases cover S1 and S2
with independent groups, identical queue revisions, reassigned addresses,
malformed topology, and an address change after an accepted insertion.

## Playback starts, continuation and silent restoration retain the endpoint

Direct-source starts, queue starts and resume validate the requested
coordinator at the exact control address before their first write. A stale
address that now belongs to another room cannot pass merely because every
speaker reports the original coordinator elsewhere in the household topology.
An initial refusal retains the original room for the existing bounded
coordinator or address recovery. The API reports `NOT_COORDINATOR` without
inventing a speaker's UPnP fault.

Lost-reply confirmation retains the same address binding before any follow-on
Play, mode change or Seek, and across resume's playing-state observation.
An ownership mismatch discovered after a write cannot authorize replay.
These observations do not lock out a later address or group change.

Silent source and queue loaders validate the binding before writes.
Snapshot source restoration, queue-bookmark scenes and saved-playlist scene
proofs also reject a different endpoint or incoherent topology.

`control_endpoint_sim` and `playback_destination_sim` exercise S1 and S2
with stale cached addresses, healthy unrelated endpoints, matching source
and playback states, real address reuse after accepted writes, and snapshot
restoration. Transport-state unit tests cover changes during either resume
confirmation read. API recovery tests retain the originally requested room,
recheck the recovered audience, and enforce one shared retry budget.

```sh
cargo test --locked -p fsonos-core --test control_endpoint_sim --test playback_destination_sim
cargo test --locked -p fsonos-core --lib control::transport_state
cargo test --locked -p fsonos-api --test recovery_target_sim
```
