# Continuous DJ queue maintenance

The DJ retires completed works once its queue exceeds `trim_queue_at`, keeping
`keep_played` tracks before the observed playing position. Retirement uses the
same `fsonos_core::control::queue_removal` implementation as manual queue edits
and logged enqueue undo. There is no separate interpretation of a deletion's
acknowledgement in the automatic feed.

## One verified deletion contract

Every removal selects complete, contiguous works behind the retained boundary,
reads a coherent queue revision, and matches the claimed tracks at their saved
positions. A fresh playback/source observation prevents an already-observed
backwards seek or source replacement from being treated as permission to trim.
The shared core checks coordinator ownership, sends one nonzero-revision range
removal, and verifies the complete resulting queue and coordinator again.

A successful reply alone is not enough: the observed queue must have exactly
the selected range removed, a changed revision, and unchanged prefix and suffix
URIs. Lost or malformed acknowledgements can succeed from that same evidence
without repeating a write. Explicit SOAP refusals remain errors. No queue
clear/rebuild, source switch, play, seek, volume or grouping operation is added.

An uncertain result, including an acknowledged deletion with contradictory or
unreadable read-back, drops the old cleanup claims before resynchronization.
Those dropped claims are absent from subsequent feed snapshots too. Positions
remain untrusted when resynchronization fails, so automatic top-up waits for a
later observation rather than completing a partial work from stale positions.

## Adjacent works share a request

Consecutive complete work claims at adjacent queue positions are coalesced.
For example, 120 completed adjacent songs can retire in one range deletion,
with one pair of complete before/after queue observations, instead of a range
request and queue scans per song. This is a change in the operation structure,
not a measured wall-clock speedup or a guarantee about speaker latency. The
protocol reader still pages large queues and final feed reconciliation still
reads the queue.

A partially queued work, an unowned entry between claims, or a work crossing
the retained boundary is never swept into a combined range. Missing positions
and overflowing ranges are ineligible; overlapping or reordered eligible
claims do not authorize cleanup. Batches stay within the protocol's object
limit and run back to front so removing a later range does not renumber an
unprocessed earlier one. Movement metadata is borrowed, not cloned or sorted.

At most eight batches are considered per playback event. Remaining old claims
stay in the feed and later events can continue retirement after reconciliation.
This bounds the number of deletion batches, not total elapsed time: individual
network calls still have their existing timeouts. The DJ's queue threshold,
lookahead, work selection, feedback and play-history policy are unchanged.

## Boundaries and verification

This does not make cleanup transactional with other controllers. Queue
revisions protect the queue edit, not a concurrent source/seek change after
preflight. Saved claims identify URI/position occurrences, not permanent queue
item identities; identical replacements before observation cannot always be
distinguished. Cleanup never gains permission to search another coordinator's
queue for the same songs.

Six new simulator tests cover shared-verifier integration. Six pure batching
tests and seven additional simulator tests cover whole-work batching, owner
gaps, incomplete work, a 120-song range, eight-batch yielding and continuation,
a changed member of a batch, and a backwards seek. The existing adjacent-work
lost-reply test now requires one combined removal, while retaining its queue,
position and no-replay assertions. Run:

```sh
cargo test --locked -p fsonos-spotify --lib feed::trim
cargo test --locked -p fsonos-core --lib control::queue_removal
cargo fmt --check
cargo check --locked --workspace --all-targets
cargo clippy --locked --workspace --all-targets -- -D warnings
```

These Rust tests were authored but not executed in the editing environment:
Cargo, rustc, rustfmt and RCH are absent, and toolchain retrieval fails DNS.
Exact source/blob hashes, narrow diffs, whitespace and patch applicability were
checked. These are not compiler, simulator, full-workspace or hardware passes.
