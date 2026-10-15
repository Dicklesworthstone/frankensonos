# Live queue browsing and coordinated commands

Open **Queue** on a room card to browse that group's revisioned queue, 25
entries at a time. The open page now follows the remote's existing coalesced
playback, DJ, topology, health, action-log and reconnect refreshes. Closed queues
make no requests. There is no additional event connection or queue polling
interval; healthy quiet queues do not reread themselves.

An incoming invalidation immediately makes the displayed snapshot read-only.
The next shared group refresh reads that page. Its current-entry marker,
titles and counts update together. A selection and destination draft survive
only when the opaque revision, total and every displayed position/URI still
match; cosmetic metadata and natural playback progress do not clear a valid
selection. The unsent source text is retained. A pending removal or movement
confirmation is consumed at invalidation, even when the eventual revision is
unchanged. Detached controls cannot act on a newly displayed snapshot.

A queue that shrinks past the open page is read once more at its new final
page (or position zero when empty). Continuing churn produces a recoverable
read failure rather than an unbounded paging loop. Background failures retain
an explicitly stale snapshot and propagate to the existing household
backoff. Manual refresh still reports its own error without replaying a write.
Live reconciliation waits for outstanding manual reads, including a corrective
page read, instead of adding overlapping automatic requests.

## One native action lane

Queue play, add, remove and move use the room card's existing playback-action
exclusion. A pending queue request blocks conflicting play/next/DJ/seek actions;
a pending native action retires the queue snapshot and its reviews. Group
splits, merges and coordinator replacements inherit outstanding exclusions
through household-qualified member rooms. Separate households remain
independent. The existing native payloads and their revision/URI preconditions
are unchanged. This is browser coordination, not an atomic speaker lock or
cross-client transaction.

After a lost or malformed queue-write reply the queue controller remains
uncertain. Passive events, automatic refreshes and hide/show do not clear that
condition. **Refresh queue** must obtain a valid fresh observation before new
queue edits are enabled. No write is retried. Closing or disposing a view does
not cancel a command already sent to the daemon. An accepted edit is separate
from observing its resulting queue. These states apply to the loaded queue
controller; this change adds no durable command-status or idempotency service.

Page hiding and page-cache suspension discard pending reviews and stop queue
work. Returning waits for the shared refresh rather than starting one poll per
card. The pane remains lazy after a topology-driven card replacement.

## Executable evidence

Run the production controller/DOM and loopback HTTP regressions with:

```sh
node --test scripts/test-remote-queue-coordination.cjs scripts/test-remote-queue-live.cjs scripts/test-remote-queue-http.cjs
```

These checks exercise the browser code and native JSON contract with synthetic
fixtures; they are not Rust-daemon, real GENA or physical-speaker acceptance.
The full existing queue suite and Rust workspace remain separate validation
gates. External changes are observed when their relevant daemon events arrive;
this client does not infer unseen edits or introduce a periodic queue survey.
