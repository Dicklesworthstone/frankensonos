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

A definite Pause/701 still preserves already-inactive playback or stops an
active unpausable stream. A lost Pause reply is not treated as a 701: it cannot
escalate into Stop. An uncertain Stop in the definite-701 fallback can itself
be confirmed by a stopped-state read. Other explicit SOAP faults are returned
unchanged; failed observations keep the original write error.

These are desired-state observations, not proof that no other controller acted
or that a particular source is still selected. Existing outer coordinator and
address recovery rules remain in force when a result cannot be established.
Next and Previous are not state-setting operations and gain no retry or inferred
success behavior. Existing policy, logging and source-loading checks remain
shared with the callers; no new endpoint or background polling is added.

```sh
cargo test --locked -p fsonos-core --lib control::transport_state
cargo test --locked -p fsonos-core --test transport_state_recovery_sim --test control_sim
```

The new simulator cases cover normal and malformed lost acknowledgements for
all three controls on both generations. They were authored but not executed
locally; the full Rust and physical-speaker validation gates remain separate.
