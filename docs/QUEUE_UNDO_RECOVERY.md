# Queue deletion and enqueue undo

Undoing an enqueue now uses a nonzero native queue revision, not wildcard
`UpdateID=0`. The saved renderer URIs must still occupy their recorded range
in one complete, coherent queue snapshot. Shifted or missing selections return
without removing anything; undo does not search for the same songs elsewhere.

The shared core removal operation sends one `RemoveTrackRangeFromQueue`, then
checks a complete queue observation: exactly the selected range is absent,
the prefix and suffix are unchanged, and the revision changed. The original
player must still be a coordinator before dispatch and after the observation.
A concurrent edit between the initial browse and the removal is rejected by
the player using the observed revision. A reported revision of zero cannot
be used for a guarded write, including after counter wrap.

An accepted deletion whose reply is lost or malformed can complete from that
read-back without repeating the write. An explicit SOAP refusal is returned
as a refusal; another client's coincidentally identical deletion is not used
to infer that a rejected write succeeded. Incomplete reads, another queue edit,
or a changed coordinator leave the result uncertain and never trigger replay.
No source reset, clear/rebuild, Play, Seek, grouping or volume operation is added.

Queue revisions are not permanent item IDs or device transactions. Identical
URI occurrences replaced at the same saved positions before the initial read
cannot be distinguished by the existing saved-undo format. This change closes
the subsequent browse/write race; it does not claim household isolation or
recoverable full-queue snapshots. An uncertain result must be inspected before
another explicit action.

Targeted checks:

```sh
cargo test --locked -p fsonos-core --lib control::queue_removal
cargo test --locked -p fsonos-core --test queue_undo_recovery_sim
cargo test --locked -p fsonos-api --lib surface::queue::removal
```

The new unit tests exercise the production core operation with scripted SOAP
responses. Simulator cases exercise S1/S2 through real loopback sockets,
including a concurrent insertion that would shift the saved range. They were
authored but not executed in the editing environment, which has no Rust
toolchain. Physical-speaker acceptance remains a separate check.
